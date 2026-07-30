use std::io;
use std::path::Path;
use std::time::Duration;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplaySize {
    columns: u32,
    rows: u32,
}

pub fn view(
    config: &Config,
    client: &mut VividClient,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let encoded = std::fs::read(path)?;
    let format = image::guess_format(&encoded).ok();
    let (width, height) = image::ImageReader::open(path)?
        .with_guessed_format()?
        .into_dimensions()?;
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
            create_raster_track(client, &surface, path, width, height)?
        }
    } else {
        create_raster_track(client, &surface, path, width, height)?
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

fn create_raster_track(
    client: &mut VividClient,
    surface: &vivid_sdk::Surface,
    path: &Path,
    width: u32,
    height: u32,
) -> io::Result<vivid_sdk::Track> {
    let rgba = image::open(path)
        .map_err(io::Error::other)?
        .into_rgba8()
        .into_raw();
    let maximum_record_body =
        vivid_protocol::media::rgba8_raw_frame_body_len(width, height).map_err(io::Error::other)?;
    let retained_pixel_charge = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "raster pixels overflow"))?;
    let configuration = TrackConfiguration {
        context_id: surface.context_id(),
        surface_id: surface.id(),
        track_id: client.allocate_id()?,
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
    };
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
    client
        .open_track_channel(&track)?
        .send_raster(1, 1, &rgba, false)?;
    Ok(track)
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
}
