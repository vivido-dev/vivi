use std::io;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::cli::Config;
use crate::client::VividClient;
use crate::protocol::HARD_MAX_RECORD_BODY;
use crate::protocol::wire::ConnectionKind;
use crate::terminal_geometry::{TerminalGeometry, cells_for_pixels, reserve_rows};

const FIT_MARGIN_COLS: u16 = 4;
const FIT_MARGIN_ROWS: u16 = 2;
const RASTER_OVERHEAD: usize = 48 + 24;

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
    let display = display_size(width, height, config.zoom, TerminalGeometry::current());

    let source_id = client.allocate_id()?;
    let node_id = client.allocate_id()?;
    let anchor_id = client.create_text_anchor()?;
    let encoded_kind = match format {
        Some(image::ImageFormat::Png) => Some(crate::protocol::messages::IMAGE_PNG),
        Some(image::ImageFormat::Jpeg) => Some(crate::protocol::messages::IMAGE_JPEG),
        _ => None,
    };
    if let Some(encoding) = encoded_kind
        .filter(|_| client.supports(crate::protocol::messages::FEATURE_ENCODED_IMAGE_V1))
    {
        let hash: [u8; 32] = Sha256::digest(&encoded).into();
        let image_config = crate::protocol::messages::ImageSourceConfig {
            source_id,
            encoding,
            width,
            height,
            encoded_length: u32::try_from(encoded.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "encoded image exceeds u32")
            })?,
            sha256: Some(hash),
        };
        let source = client.create_image_source(&image_config)?;
        client.place_source(source_id, node_id, anchor_id, display.columns, display.rows)?;
        if !config.is_dry_run() {
            reserve_rows(display.rows)?;
        }
        let mut sender = client.open_media_sender(source, ConnectionKind::Blob)?;
        sender.send_image(&encoded)?;
    } else {
        let image = image::open(path)?;
        let rgba = image.into_rgba8().into_raw();
        let maximum_pixels = (HARD_MAX_RECORD_BODY as usize)
            .saturating_sub(RASTER_OVERHEAD)
            .saturating_div(4);
        if rgba.len() / 4 > maximum_pixels {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is {width}x{height}; its uncompressed frame exceeds the Vivid 1.0 \
                     raster record limit",
                    path.display()
                ),
            )
            .into());
        }
        let source = client.create_raster_source(source_id, width, height)?;
        client.place_source(source_id, node_id, anchor_id, display.columns, display.rows)?;
        if !config.is_dry_run() {
            reserve_rows(display.rows)?;
        }
        let mut sender = client.open_media_sender(source, ConnectionKind::Raster)?;
        sender.send_raster(1, 1, width, height, &rgba)?;
    }
    client.verbose(format_args!(
        "image {}: {width}x{height} RGBA -> {}x{} cells",
        path.display(),
        display.columns,
        display.rows
    ));
    Ok(())
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
