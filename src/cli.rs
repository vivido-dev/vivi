use std::io;
use std::path::PathBuf;

use clap::Parser;

use crate::terminal_geometry::Pane;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "vivi",
    version,
    about = "Display images and play video or audio in Vivido",
    long_about = "A Vivid Protocol image viewer and local audio/video player. It connects to the private \
                  per-window endpoint inherited from Vivido; --dry-run and --trace-dir generate \
                  deterministic wire fixtures without a presenter."
)]
pub struct Config {
    /// Image (including SVG), video, MP3, M4A, or WAV files to display or play.
    #[arg(required = true)]
    pub files: Vec<PathBuf>,

    /// Zoom multiplier applied to the media's natural pixel size.
    #[arg(short = 'z', long, default_value_t = 1.0)]
    pub zoom: f32,

    /// Keep video and audio playback inline instead of using the full terminal window.
    #[arg(short = 'i', long)]
    pub inline: bool,

    /// Vivid control endpoint, normally inherited as VIVID_ENDPOINT_CONTROL.
    #[arg(long, env = "VIVID_ENDPOINT_CONTROL")]
    pub control_endpoint: Option<String>,

    /// Optional realtime endpoint, normally inherited as VIVID_ENDPOINT_REALTIME.
    #[arg(long, env = "VIVID_ENDPOINT_REALTIME")]
    pub realtime_endpoint: Option<String>,

    /// Alternate endpoint for media connections, normally inherited as VIVID_ENDPOINT_BULK.
    #[arg(long, env = "VIVID_ENDPOINT_BULK")]
    pub bulk_endpoint: Option<String>,

    /// Build the complete request stream without connecting to Vivido.
    #[arg(long)]
    pub dry_run: bool,

    /// Write each Vivid connection to a separate file in this directory.
    /// Implies --dry-run.
    #[arg(long, value_name = "DIRECTORY")]
    pub trace_dir: Option<PathBuf>,

    /// Print surface, track, channel, placement, and playback progress.
    #[arg(short, long)]
    pub verbose: bool,

    /// Exit after video submission without waiting or starting local audio.
    #[arg(long)]
    pub no_wait: bool,

    /// Show images full window and wait for a key; video and audio already play full window.
    #[arg(short = 'f', long, conflicts_with = "inline")]
    pub fullscreen: bool,

    /// Preview in a file manager pane: the arguments are FILE COLUMNS ROWS [COLUMN ROW].
    ///
    /// The media stays in that grid rectangle until stdin closes. Stdin lines `hide` and `show`
    /// toggle it while something covers the pane. Videos loop, muted unless --sound is given.
    #[arg(long, conflicts_with_all = ["inline", "fullscreen", "no_wait"])]
    pub preview: bool,

    /// Play the sound of videos shown with --preview.
    #[arg(long, requires = "preview")]
    pub sound: bool,
}

impl Config {
    pub fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.zoom.is_finite() || self.zoom <= 0.0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--zoom must be a finite number greater than zero",
            )
            .into());
        }

        if !self.is_dry_run() && self.control_endpoint.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "VIVID_ENDPOINT_CONTROL is not set; run Vivi inside Vivido or use --dry-run \
                 (optionally with --trace-dir)",
            )
            .into());
        }

        Ok(())
    }

    pub fn is_dry_run(&self) -> bool {
        self.dry_run || self.trace_dir.is_some()
    }

    /// The file and pane of a `--preview` run.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` unless the arguments are `FILE COLUMNS ROWS [COLUMN ROW]` with a
    /// nonempty pane.
    pub fn preview_target(&self) -> io::Result<(PathBuf, Pane)> {
        let invalid =
            |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message.to_owned());
        let [file, cells @ ..] = self.files.as_slice() else {
            return Err(invalid("--preview needs FILE COLUMNS ROWS [COLUMN ROW]"));
        };
        if !matches!(cells.len(), 2 | 4) {
            return Err(invalid("--preview needs FILE COLUMNS ROWS [COLUMN ROW]"));
        }
        let mut numbers = [0_u32; 4];
        for (number, cell) in numbers.iter_mut().zip(cells) {
            *number = cell
                .to_str()
                .and_then(|text| text.parse().ok())
                .ok_or_else(|| invalid("--preview pane sizes and positions are cell counts"))?;
        }
        let [columns, rows, column, row] = numbers;
        if columns == 0 || rows == 0 {
            return Err(invalid("--preview pane must be at least one cell"));
        }
        Ok((
            file.clone(),
            Pane {
                column,
                row,
                columns,
                rows,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            files: vec![PathBuf::from("image.png")],
            zoom: 1.0,
            inline: false,
            control_endpoint: None,
            realtime_endpoint: None,
            bulk_endpoint: None,
            dry_run: true,
            trace_dir: None,
            verbose: false,
            no_wait: false,
            fullscreen: false,
            preview: false,
            sound: false,
        }
    }

    #[test]
    fn dry_run_does_not_require_endpoint_or_root_secret() {
        assert!(config().validate().is_ok());
    }

    #[test]
    fn rejects_invalid_zoom() {
        let mut config = config();
        config.zoom = f32::NAN;
        assert!(config.validate().is_err());
    }

    #[test]
    fn accepts_only_protocol_1_5_discovery_flags() {
        let parsed = Config::try_parse_from([
            "vivi",
            "--dry-run",
            "--control-endpoint",
            "unix:/control",
            "--realtime-endpoint",
            "unix:/realtime",
            "--bulk-endpoint",
            "unix:/bulk",
            "image.png",
        ])
        .unwrap();
        assert_eq!(parsed.control_endpoint.as_deref(), Some("unix:/control"));
        assert_eq!(parsed.realtime_endpoint.as_deref(), Some("unix:/realtime"));
        assert!(Config::try_parse_from(["vivi", "--endpoint", "unix:/old", "image.png"]).is_err());
        assert!(Config::try_parse_from(["vivi", "--token", "secret", "image.png"]).is_err());
    }

    #[test]
    fn preview_takes_the_pane_vfm_appends_after_the_file() {
        let parse = |args: &[&str]| {
            Config::try_parse_from(["vivi", "--preview"].iter().chain(args))
                .unwrap()
                .preview_target()
        };
        let pane = |column, row, columns, rows| Pane {
            column,
            row,
            columns,
            rows,
        };
        let (file, at) = parse(&["clip.mp4", "48", "20", "51", "2"]).unwrap();
        assert_eq!((file, at), (PathBuf::from("clip.mp4"), pane(51, 2, 48, 20)));
        assert_eq!(parse(&["a.png", "10", "5"]).unwrap().1, pane(0, 0, 10, 5));
        for bad in [
            &["a.png"][..],
            &["a.png", "10"],
            &["a.png", "0", "5"],
            &["a.png", "x", "5"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert!(Config::try_parse_from(["vivi", "--preview", "-i", "a.png", "1", "1"]).is_err());
        assert!(Config::try_parse_from(["vivi", "--sound", "a.mp4"]).is_err());
    }

    #[test]
    fn inline_short_option_preserves_the_legacy_mode() {
        let parsed = Config::try_parse_from(["vivi", "--dry-run", "-i", "clip.mp4"]).unwrap();
        assert!(parsed.inline);
    }
}
