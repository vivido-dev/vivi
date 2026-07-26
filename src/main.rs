mod audio_player;
mod audio_streamer;
mod cli;
mod client;
mod ffmpeg;
mod image_viewer;
mod terminal_geometry;
mod video_player;

pub use vivid_protocol as protocol;

use std::path::Path;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::Config;
use crate::client::{VividClient, producer_config};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaHint {
    Video,
    Audio,
    Unknown,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vivi: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::parse();
    config.validate()?;

    let mut client = VividClient::connect(&producer_config(&config))?;
    for file in &config.files {
        let result = match media_hint(file) {
            MediaHint::Video => video_player::play(&config, &mut client, file),
            MediaHint::Audio => play_audio(&config, &mut client, file),
            MediaHint::Unknown => match image_viewer::view(&config, &mut client, file) {
                Ok(()) => Ok(()),
                Err(image_error) => match video_player::play(&config, &mut client, file) {
                    Ok(()) => Ok(()),
                    Err(video_error) => match play_audio(&config, &mut client, file) {
                        Ok(()) => Ok(()),
                        Err(audio_error) => Err(std::io::Error::other(format!(
                            "could not read {} as an image ({image_error}), video ({video_error}), or audio ({audio_error})",
                            file.display()
                        ))
                        .into()),
                    },
                },
            },
        };

        result?;
    }

    client.goodbye()?;
    Ok(())
}

fn media_hint(path: &Path) -> MediaHint {
    if looks_like_video(path) {
        MediaHint::Video
    } else if looks_like_audio(path) {
        MediaHint::Audio
    } else {
        MediaHint::Unknown
    }
}

fn play_audio(
    config: &Config,
    client: &mut VividClient,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_audio_mode(config)?;
    if client.supports(crate::protocol::messages::FEATURE_AUDIO_ACCESS_UNIT_V1) {
        match audio_streamer::play(client, path) {
            Ok(()) => return Ok(()),
            Err(error) if std::env::var_os("VIVID_REMOTE").is_some() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    format!("remote audio negotiation failed: {error}"),
                )
                .into());
            }
            Err(error) if config.is_dry_run() => return Err(error),
            Err(error) => client.verbose(format_args!(
                "presenter audio failed for {}: {error}; using local audio output",
                path.display()
            )),
        }
    }
    if std::env::var_os("VIVID_REMOTE").is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "remote audio requires a presenter with audio-access-unit-v1",
        )
        .into());
    }
    audio_player::play(path)?;
    Ok(())
}

fn validate_audio_mode(config: &Config) -> std::io::Result<()> {
    if config.no_wait {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--no-wait cannot play an audio-only file because audio output is local",
        ));
    }
    Ok(())
}

fn looks_like_video(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "wmv" | "m4v" | "mpg" | "mpeg" | "3gp"
    )
}

fn looks_like_audio(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "mp3" | "m4a" | "wav" | "flac" | "ogg" | "opus"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_common_video_extensions_case_insensitively() {
        assert!(looks_like_video(Path::new("clip.MKV")));
        assert!(looks_like_video(Path::new("clip.mp4")));
        assert!(!looks_like_video(Path::new("photo.png")));
    }

    #[test]
    fn recognizes_supported_audio_extensions_case_insensitively() {
        assert!(looks_like_audio(Path::new("song.MP3")));
        assert!(looks_like_audio(Path::new("recording.m4a")));
        assert!(looks_like_audio(Path::new("sample.WAV")));
        assert!(!looks_like_audio(Path::new("photo.png")));
    }

    #[test]
    fn extension_hints_route_audio_before_generic_decoder_fallback() {
        assert_eq!(media_hint(Path::new("clip.mp4")), MediaHint::Video);
        assert_eq!(media_hint(Path::new("song.m4a")), MediaHint::Audio);
        assert_eq!(media_hint(Path::new("extensionless")), MediaHint::Unknown);
    }

    #[test]
    fn audio_only_rejects_no_wait_but_allows_deterministic_dry_run() {
        let mut config = Config {
            files: vec!["song.mp3".into()],
            zoom: 1.0,
            endpoint: None,
            bulk_endpoint: None,
            token: None,
            dry_run: false,
            trace_dir: None,
            verbose: false,
            no_wait: true,
        };
        assert!(validate_audio_mode(&config).is_err());
        config.no_wait = false;
        config.dry_run = true;
        assert!(validate_audio_mode(&config).is_ok());
    }
}
