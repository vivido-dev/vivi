use std::io;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use resvg::{
    tiny_skia::{Pixmap, Transform},
    usvg::{ImageHrefResolver, ImageKind, Options, Tree},
};
use sha2::{Digest, Sha256};
use vivid_protocol::messages::LaneClass;
use vivid_protocol::track::{KindConfiguration, TrackConfiguration, TrackMode};
use vivid_sdk::{
    CoordinateModel, ImageConfiguration, MILESTONE_OUTPUT_READY, MILESTONE_PRESENTED,
    RasterConfiguration, RequestMetadata, SlotBinding, SurfaceDefinition, SurfaceDescriptor,
    SurfaceRole, TrackWaitCondition,
};

use crate::cli::Config;
use crate::client::VividClient;
use crate::terminal_geometry::{TerminalGeometry, cells_for_pixels, place_surface, reserve_rows};

const FIT_MARGIN_COLS: u16 = 4;
const FIT_MARGIN_ROWS: u16 = 2;
const PRESENTATION_TIMEOUT: Duration = Duration::from_secs(30);
const SLOT_RASTER: u64 = 3;
const SLOT_POSTER: u64 = 4;
const IMAGE_PNG: u64 = 1;
const IMAGE_JPEG: u64 = 2;
const MAX_NESTED_SVG_DEPTH: u8 = 8;

static SVG_FONT_DATABASE: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplaySize {
    columns: u32,
    rows: u32,
}

#[derive(Debug)]
struct RasterImage {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

pub fn view(
    config: &Config,
    client: &mut VividClient,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let encoded = std::fs::read(path)?;
    let format = image::guess_format(&encoded).ok();
    let svg = if format.is_none() {
        Some(render_svg(path, &encoded)?)
    } else {
        None
    };
    let (width, height) = match &svg {
        Some(svg) => (svg.width, svg.height),
        None => image::ImageReader::open(path)?
            .with_guessed_format()?
            .into_dimensions()?,
    };
    let display = display_size(
        width,
        height,
        config.zoom,
        TerminalGeometry::settled_presenter(client),
    );
    let surface_id = client.allocate_id()?;
    let node_id = client.allocate_id()?;
    let context_id = client.info().root_context_id;
    let surface = client.create_surface(
        image_surface(context_id, surface_id, path, width, height),
        &RequestMetadata::default(),
    )?;
    place_surface(client, &surface, node_id, display.columns, display.rows)?;
    if !config.is_dry_run() {
        reserve_rows(display.rows)?;
    }

    let encoded_kind = match format {
        Some(image::ImageFormat::Png) => Some(IMAGE_PNG),
        Some(image::ImageFormat::Jpeg) => Some(IMAGE_JPEG),
        _ => None,
    };
    let track = if let Some(encoding) = encoded_kind {
        let configuration =
            encoded_image_track(client, &surface, encoding, width, height, &encoded)?;
        let probe = probe_configuration(&configuration);
        if client.probe_track(&probe)?.supported {
            let track = client.create_track(configuration, &RequestMetadata::default())?;
            if track.connection_required()? {
                client.open_track_channel(&track)?.send_image(&encoded)?;
            } else {
                client.verbose(format_args!(
                    "image {}: presenter cache hit; skipped {} encoded bytes",
                    path.display(),
                    encoded.len()
                ));
            }
            track
        } else {
            let rgba = decode_raster(&encoded, format, width, height)?;
            create_raster_track(client, &surface, width, height, &rgba)?
        }
    } else {
        let rgba = match svg {
            Some(svg) => svg.rgba,
            None => decode_raster(&encoded, format, width, height)?,
        };
        create_raster_track(client, &surface, width, height, &rgba)?
    };

    client.wait_track(
        &track,
        TrackWaitCondition::MilestoneSet,
        Some(MILESTONE_OUTPUT_READY),
        timeout_us(PRESENTATION_TIMEOUT),
    )?;
    client.activate_tracks(
        &surface,
        &[SlotBinding {
            slot: track.configuration()?.slot,
            track_id: track.id(),
            expected_channel_generation: track.channel_generation(),
            required_milestone: MILESTONE_OUTPUT_READY,
        }],
        &RequestMetadata::default(),
    )?;
    if !config.no_wait {
        client.wait_track(
            &track,
            TrackWaitCondition::MilestoneSet,
            Some(MILESTONE_PRESENTED),
            timeout_us(PRESENTATION_TIMEOUT),
        )?;
    }
    client.verbose(format_args!(
        "image surface {surface_id}: {} is {width}x{height}, presented at {}x{} cells",
        path.display(),
        display.columns,
        display.rows
    ));
    Ok(())
}

fn image_surface(
    context_id: u64,
    surface_id: u64,
    path: &Path,
    width: u32,
    height: u32,
) -> SurfaceDefinition {
    SurfaceDefinition {
        context_id,
        surface_id,
        semantic_profile: vivid_sdk::GENERIC_CONTENT.into(),
        coordinate_model: CoordinateModel::DesktopLogicalPixels,
        logical_width: u64::from(width),
        logical_height: u64::from(height),
        scale_numerator: 1,
        scale_denominator: 1,
        rotation: 0,
        descriptor: SurfaceDescriptor {
            role: SurfaceRole::Figure,
            title: bounded_title(path),
            semantic_content_revision: 1,
            semantic_availability: 0,
            locator_hint: String::new(),
        },
        policy: 0,
        profile_parameters: vec![],
    }
}

fn encoded_image_track(
    client: &VividClient,
    surface: &vivid_sdk::Surface,
    encoding: u64,
    width: u32,
    height: u32,
    encoded: &[u8],
) -> io::Result<TrackConfiguration> {
    let encoded_length = u32::try_from(encoded.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "encoded image exceeds u32"))?;
    let retained_pixel_charge = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "image pixels overflow"))?;
    Ok(TrackConfiguration {
        context_id: surface.context_id(),
        surface_id: surface.id(),
        track_id: client.allocate_id()?,
        slot: SLOT_POSTER,
        mode: TrackMode::Live,
        lane: LaneClass::Bulk,
        maximum_record_body: encoded_length,
        maximum_rate_millihertz: 1,
        maximum_encoded_bits_per_second: u64::from(encoded_length).saturating_mul(8).max(1),
        maximum_records_per_second: 1,
        maximum_inflight_body_bytes: u64::from(encoded_length),
        kind: KindConfiguration::EncodedImage(ImageConfiguration {
            encoding,
            width,
            height,
            encoded_length,
            sha256: Some(Sha256::digest(encoded).into()),
            cache_lookup: true,
        }),
        target_latency_us: 0,
        maximum_latency_us: 1_000_000,
        retained_pixel_charge,
    })
}

/// The immutable configuration for a full-frame RGBA raster track on `surface`.
pub(crate) fn raster_track_configuration(
    session: &vivid_sdk::Session,
    surface: &vivid_sdk::Surface,
    width: u32,
    height: u32,
) -> io::Result<TrackConfiguration> {
    let (maximum_record_body, retained_pixel_charge) = raster_limits(width, height)?;
    Ok(TrackConfiguration {
        context_id: surface.context_id(),
        surface_id: surface.id(),
        track_id: session.allocate_id()?,
        slot: SLOT_RASTER,
        mode: TrackMode::Live,
        lane: LaneClass::Bulk,
        maximum_record_body,
        maximum_rate_millihertz: 1,
        maximum_encoded_bits_per_second: u64::from(maximum_record_body).saturating_mul(8).max(1),
        maximum_records_per_second: 1,
        maximum_inflight_body_bytes: u64::from(maximum_record_body),
        kind: KindConfiguration::Raster(RasterConfiguration {
            width,
            height,
            alpha_mode: 1,
            delta_enabled: false,
            maximum_delta_operations: 1,
            zstd_enabled: false,
        }),
        target_latency_us: 0,
        maximum_latency_us: 1_000_000,
        retained_pixel_charge,
    })
}

/// Sends one full RGBA frame as the raster track's first (and typically only) record.
pub(crate) fn send_full_raster_frame(
    session: &mut vivid_sdk::Session,
    track: &vivid_sdk::Track,
    rgba: &[u8],
) -> io::Result<()> {
    session
        .open_track_channel(track)?
        .send_raster(1, 1, rgba, false)?;
    Ok(())
}

fn create_raster_track(
    client: &mut VividClient,
    surface: &vivid_sdk::Surface,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> io::Result<vivid_sdk::Track> {
    let expected_length = usize::try_from(u64::from(width) * u64::from(height))
        .ok()
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "raster size overflow"))?;
    if rgba.len() != expected_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "RGBA data length does not match raster dimensions",
        ));
    }
    let configuration = raster_track_configuration(client, surface, width, height)?;
    if !client
        .probe_track(&probe_configuration(&configuration))?
        .supported
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "presenter rejected both encoded-image and raster track configurations",
        ));
    }
    let track = client.create_track(configuration, &RequestMetadata::default())?;
    send_full_raster_frame(client, &track, rgba)?;
    Ok(track)
}

fn decode_raster(
    encoded: &[u8],
    format: Option<image::ImageFormat>,
    width: u32,
    height: u32,
) -> io::Result<Vec<u8>> {
    raster_limits(width, height)?;
    let format = format
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown raster image format"))?;
    image::load_from_memory_with_format(encoded, format)
        .map_err(io::Error::other)
        .map(|image| image.into_rgba8().into_raw())
}

fn render_svg(path: &Path, encoded: &[u8]) -> io::Result<RasterImage> {
    let canonical_path = std::fs::canonicalize(path)?;
    let root = canonical_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "SVG has no parent directory"))?
        .to_path_buf();
    let tree = parse_svg_tree(encoded, &root, &root, 0).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not parse SVG: {error}"),
        )
    })?;
    let size = tree.size().to_int_size();
    let width = size.width();
    let height = size.height();
    raster_limits(width, height)?;

    let mut pixmap = Pixmap::new(width, height)
        .ok_or_else(|| io::Error::other("could not allocate SVG raster"))?;
    resvg::render(&tree, Transform::default(), &mut pixmap.as_mut());
    Ok(RasterImage {
        width,
        height,
        rgba: pixmap.take_demultiplied(),
    })
}

fn parse_svg_tree(
    encoded: &[u8],
    root: &Path,
    resource_directory: &Path,
    depth: u8,
) -> Result<Tree, resvg::usvg::Error> {
    let options = svg_options(root, resource_directory, depth);
    Tree::from_data(encoded, &options)
}

fn svg_options(root: &Path, resource_directory: &Path, depth: u8) -> Options<'static> {
    let confined_root = root.to_path_buf();
    let current_directory = resource_directory.to_path_buf();
    let resolve_string = Box::new(move |href: &str, _options: &Options| {
        if depth >= MAX_NESTED_SVG_DEPTH {
            return None;
        }
        let path = confined_resource_path(&confined_root, &current_directory, href)?;
        let data = Arc::new(std::fs::read(&path).ok()?);
        match image::guess_format(&data).ok() {
            Some(image::ImageFormat::Jpeg) => Some(ImageKind::JPEG(data)),
            Some(image::ImageFormat::Png) => Some(ImageKind::PNG(data)),
            Some(image::ImageFormat::Gif) => Some(ImageKind::GIF(data)),
            Some(image::ImageFormat::WebP) => Some(ImageKind::WEBP(data)),
            _ => {
                let directory = path.parent()?;
                parse_svg_tree(&data, &confined_root, directory, depth + 1)
                    .ok()
                    .map(ImageKind::SVG)
            }
        }
    });
    Options {
        resources_dir: Some(resource_directory.to_path_buf()),
        image_href_resolver: ImageHrefResolver {
            resolve_data: ImageHrefResolver::default_data_resolver(),
            resolve_string,
        },
        fontdb: svg_font_database(),
        ..Options::default()
    }
}

fn svg_font_database() -> Arc<resvg::usvg::fontdb::Database> {
    SVG_FONT_DATABASE
        .get_or_init(|| {
            let mut database = resvg::usvg::fontdb::Database::new();
            database.load_system_fonts();
            Arc::new(database)
        })
        .clone()
}

fn confined_resource_path(root: &Path, current: &Path, href: &str) -> Option<std::path::PathBuf> {
    use std::path::Component;

    let relative = Path::new(href);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return None;
    }
    let canonical = std::fs::canonicalize(current.join(relative)).ok()?;
    canonical.starts_with(root).then_some(canonical)
}

fn raster_limits(width: u32, height: u32) -> io::Result<(u32, u64)> {
    let maximum_record_body =
        vivid_protocol::media::rgba8_raw_frame_body_len(width, height).map_err(io::Error::other)?;
    if width > 8192 || height > 8192 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "raster dimensions exceed the Vivid 8192-pixel limit",
        ));
    }
    let retained_pixel_charge = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "raster pixels overflow"))?;
    Ok((maximum_record_body, retained_pixel_charge))
}

fn probe_configuration(configuration: &TrackConfiguration) -> TrackConfiguration {
    let mut probe = configuration.clone();
    probe.track_id = 0;
    probe
}

fn bounded_title(path: &Path) -> String {
    let title = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("image");
    title.chars().take(256).collect()
}

fn timeout_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn display_size(width: u32, height: u32, zoom: f32, geometry: TerminalGeometry) -> DisplaySize {
    let desired_width = (width as f64 * f64::from(zoom)).round().max(1.0);
    let desired_height = (height as f64 * f64::from(zoom)).round().max(1.0);
    let maximum_width = f64::from(geometry.drawable_width_px(FIT_MARGIN_COLS));
    let maximum_height = f64::from(geometry.drawable_height_px(FIT_MARGIN_ROWS));
    let scale = (maximum_width / desired_width)
        .min(maximum_height / desired_height)
        .min(1.0);
    let target_width = (desired_width * scale).round().clamp(1.0, maximum_width) as u32;
    let target_height = (desired_height * scale).round().clamp(1.0, maximum_height) as u32;

    DisplaySize {
        columns: cells_for_pixels(target_width, geometry.cell_width_px),
        rows: cells_for_pixels(target_height, geometry.cell_height_px),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new() -> Self {
            let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("vivi-svg-test-{}-{sequence}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn render_temporary_svg(
        directory: &TemporaryDirectory,
        source: &[u8],
    ) -> io::Result<RasterImage> {
        let path = directory.path().join("image.svg");
        std::fs::write(&path, source)?;
        render_svg(&path, source)
    }

    #[test]
    fn natural_size_is_preserved_when_it_fits() {
        let geometry = TerminalGeometry::with_cell_size(120, 40, 10, 20);
        assert_eq!(
            display_size(640, 360, 1.0, geometry),
            DisplaySize {
                columns: 64,
                rows: 18
            }
        );
    }

    #[test]
    fn large_media_shrinks_to_terminal_margin() {
        let geometry = TerminalGeometry::with_cell_size(80, 24, 10, 20);
        assert_eq!(
            display_size(1280, 720, 1.0, geometry),
            DisplaySize {
                columns: 76,
                rows: 22
            }
        );
    }

    #[test]
    fn svg_renders_declared_dimensions_color_and_transparency() {
        let directory = TemporaryDirectory::new();
        let image = render_temporary_svg(
            &directory,
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="1"><rect width="1" height="1" fill="#ff0000"/></svg>"##,
        )
        .unwrap();

        assert_eq!((image.width, image.height), (2, 1));
        assert_eq!(image.rgba, [255, 0, 0, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn svg_uses_resvg_default_size_when_dimensions_are_omitted() {
        let directory = TemporaryDirectory::new();
        let image = render_temporary_svg(
            &directory,
            br#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="100%" height="100%" fill="blue"/></svg>"#,
        )
        .unwrap();

        assert_eq!((image.width, image.height), (100, 100));
    }

    #[test]
    fn malformed_and_oversized_svg_inputs_are_rejected() {
        let malformed_directory = TemporaryDirectory::new();
        let error = render_temporary_svg(&malformed_directory, b"<svg").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let oversized_directory = TemporaryDirectory::new();
        let error = render_temporary_svg(
            &oversized_directory,
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="5000" height="5000"/>"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("TooLarge"));
    }

    #[test]
    fn svg_can_render_an_image_from_its_directory() {
        let directory = TemporaryDirectory::new();
        image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 255, 0, 255]))
            .save(directory.path().join("asset.png"))
            .unwrap();
        let image = render_temporary_svg(
            &directory,
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="asset.png" width="1" height="1"/></svg>"#,
        )
        .unwrap();

        assert_eq!(image.rgba, [0, 255, 0, 255]);
    }

    #[test]
    fn svg_resource_paths_are_confined_without_parent_traversal() {
        let directory = TemporaryDirectory::new();
        let root = directory.path().join("root");
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(root.join("asset.png"), b"asset").unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let child = std::fs::canonicalize(child).unwrap();

        assert_eq!(
            confined_resource_path(&root, &root, "asset.png"),
            Some(root.join("asset.png"))
        );
        assert!(confined_resource_path(&root, &child, "../asset.png").is_none());
        assert!(
            confined_resource_path(&root, &root, root.join("asset.png").to_str().unwrap())
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn svg_resource_symlinks_cannot_escape_the_directory_tree() {
        use std::os::unix::fs::symlink;

        let directory = TemporaryDirectory::new();
        let root = directory.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = directory.path().join("outside.png");
        std::fs::write(&outside, b"outside").unwrap();
        symlink(&outside, root.join("escape.png")).unwrap();
        let root = std::fs::canonicalize(root).unwrap();

        assert!(confined_resource_path(&root, &root, "escape.png").is_none());
    }
}
