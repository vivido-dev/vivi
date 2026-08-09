use std::fmt;
use std::io;
use std::ops::{Deref, DerefMut};

use vivid_sdk::{ProducerAuthentication, ProducerConfig, Session};

use crate::cli::Config;
use crate::protocol::registry::{
    AUDIO_GAIN, CORE_CONTROL, LIVE_MEDIA, OBSERVABILITY, TERMINAL_SURFACE, TIMED_MEDIA,
};

pub struct VividClient {
    session: Session,
    verbose: bool,
    offline: bool,
}

impl VividClient {
    pub fn connect(config: &Config) -> io::Result<Self> {
        let session = Session::connect(producer_config(config))?;
        if session.info().target_profile != TERMINAL_SURFACE {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "presenter did not select terminal-surface-v1",
            ));
        }
        if let Err(error) = crate::terminal_geometry::TerminalGeometry::from_target_descriptor(
            &session.info().target_descriptor,
        ) && error.kind() != io::ErrorKind::WouldBlock
        {
            return Err(error);
        }
        Ok(Self {
            session,
            verbose: config.verbose,
            offline: config.is_dry_run(),
        })
    }

    pub fn verbose(&self, message: fmt::Arguments<'_>) {
        if self.verbose {
            eprintln!("vivi: {message}");
        }
    }

    pub fn close(self) -> io::Result<()> {
        self.session.close()
    }

    pub fn is_offline(&self) -> bool {
        self.offline
    }
}

impl Deref for VividClient {
    type Target = Session;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

impl DerefMut for VividClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.session
    }
}

pub fn producer_config(config: &Config) -> ProducerConfig {
    ProducerConfig {
        endpoint_control: config.control_endpoint.clone(),
        endpoint_realtime: config.realtime_endpoint.clone(),
        endpoint_bulk: config.bulk_endpoint.clone(),
        authentication: ProducerAuthentication::RootFromEnvironment,
        producer_name: "vivi".into(),
        producer_version: env!("CARGO_PKG_VERSION").into(),
        target_profile: TERMINAL_SURFACE.into(),
        required_profiles: vec![
            LIVE_MEDIA.into(),
            OBSERVABILITY.into(),
            TERMINAL_SURFACE.into(),
            TIMED_MEDIA.into(),
            CORE_CONTROL.into(),
        ],
        optional_profiles: vec![AUDIO_GAIN.into()],
        dry_run: config.dry_run,
        trace_dir: config.trace_dir.clone(),
        ..ProducerConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn producer_profiles_are_strictly_sorted_and_prerequisite_closed() {
        let config = Config {
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
        };
        producer_config(&config).validate().unwrap();
    }
}
