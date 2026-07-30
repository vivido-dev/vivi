use std::path::PathBuf;

use clap::Parser;

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
    /// Image, video, MP3, M4A, or WAV files to display or play.
    #[arg(required = true)]
    pub files: Vec<PathBuf>,

    /// Zoom multiplier applied to the media's natural pixel size.
    #[arg(short = 'z', long, default_value_t = 1.0)]
    pub zoom: f32,

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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            files: vec![PathBuf::from("image.png")],
            zoom: 1.0,
            control_endpoint: None,
            realtime_endpoint: None,
            bulk_endpoint: None,
            dry_run: true,
            trace_dir: None,
            verbose: false,
            no_wait: false,
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
}
