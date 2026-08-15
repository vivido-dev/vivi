use std::fmt;
use std::io;
use std::ops::{Deref, DerefMut};
use std::thread;
use std::time::{Duration, Instant};

use vivid_protocol::resource::{Resource, TokenBucket};
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

/// How much faster than its own timeline a timed track may be delivered.
///
/// Enough to cover any ordinary key-frame interval in a single burst, since the token bucket a
/// presenter shapes with begins full and holds one second of the declared rate.
const TIMELINE_CATCHUP: u64 = 8;

/// The delivery ceilings a timed track declares, given the timeline rates measured from the file.
///
/// Media §5.2 makes the declared rates the sender's pacing contract and lets a presenter shape
/// ingress to them. Timed media is never delivered at its timeline rate: a seek has to hand over
/// every access unit from the preceding random-access point before the first picture at the target
/// can be decoded, so declaring the timeline rate makes that pre-roll take exactly as long as the
/// material it covers — a five-second key-frame interval costs a five-second seek. Declare the
/// catch-up headroom the producer actually uses, bounded by what this session reserves, and keep
/// the timeline rate itself (`maximum_rate_millihertz`, and the decoded pixels reserved from it)
/// honest.
pub fn catchup_delivery_rates(
    session: &Session,
    records_per_second: u64,
    bits_per_second: u64,
) -> (u64, u64) {
    let contract = &session.info().resource_contract;
    (
        catchup_within(
            records_per_second,
            contract.get(Resource::MediaRecordsPerSecond),
        ),
        catchup_within(
            bits_per_second,
            contract.get(Resource::EncodedBitsPerSecond),
        ),
    )
}

/// One rate's catch-up ceiling: never above what the session reserves, never below the timeline.
fn catchup_within(timeline: u64, reserved: u64) -> u64 {
    timeline
        .saturating_mul(TIMELINE_CATCHUP)
        .min(reserved.max(timeline))
        .max(1)
}

/// The longest a single record waits for the timeline rate before the caller gets control back.
const PACING_SLICE: Duration = Duration::from_millis(50);

/// Paces media that will be presented at the rate its own timeline plays.
///
/// The declared ceiling says what a presenter must *admit*. It is not a rate to deliver at: a
/// timed presenter buffers what it needs and paces the rest against its clock, so media handed
/// over faster than it plays only accumulates in whatever queue sits between the producer and the
/// screen. Attached directly that queue is the presenter's own flow window, which pushes back. A
/// nested presenter relays through a bounded queue that drops when it overflows, and every drop
/// costs a recovery episode, so running ahead there is how a producer stalls its own playback.
///
/// The declared headroom is left for the traffic that genuinely has to catch up — the pre-roll a
/// seek must hand over before its target can be shown, which is not admitted through here at all.
pub struct DeliveryPacer {
    bucket: TokenBucket,
    updated: Instant,
}

impl DeliveryPacer {
    pub fn new(records_per_second: u64) -> Self {
        Self {
            // Capacity is `max(rate, charge)`, so a one-record charge asks for one second of burst.
            bucket: TokenBucket::new(records_per_second.max(1), 1),
            updated: Instant::now(),
        }
    }

    /// Block until one more record fits the timeline rate.
    pub fn admit_record(&mut self) {
        loop {
            let now = Instant::now();
            let elapsed = now.saturating_duration_since(self.updated);
            self.updated = now;
            if self.bucket.replenish(elapsed).is_err() {
                return;
            }
            match self.bucket.time_until(1) {
                Ok(None) => {
                    let _ = self.bucket.charge(1);
                    return;
                }
                Ok(Some(wait)) => thread::sleep(wait.min(PACING_SLICE)),
                Err(_) => return,
            }
        }
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
    fn timed_delivery_asks_for_catch_up_headroom_within_the_session_contract() {
        // A seek hands over the whole random-access unit before its target can be shown. Declaring
        // only the timeline rate makes a presenter shape that pre-roll to real time, which is a
        // seek as long as the material it covers.
        assert_eq!(catchup_within(30, 4_000), 240);
        // Never past what the session reserves, and never below the timeline the file really has:
        // an under-provisioned contract is the presenter's decision to make at track creation.
        assert_eq!(catchup_within(600, 4_000), 4_000);
        assert_eq!(catchup_within(600, 100), 600);
        assert_eq!(catchup_within(0, 4_000), 1);
        assert_eq!(catchup_within(u64::MAX, u64::MAX), u64::MAX);
    }

    #[test]
    fn presented_media_is_delivered_at_the_rate_its_timeline_plays() {
        // One second of burst is admitted at once, which is what fills a presenter's prebuffer.
        let mut pacer = DeliveryPacer::new(100);
        let burst = Instant::now();
        for _ in 0..100 {
            pacer.admit_record();
        }
        assert!(
            burst.elapsed() < Duration::from_millis(50),
            "the first second of records is the burst, not a wait"
        );
        // Past the burst the rate is the timeline's. Regression: delivering at the declared
        // ceiling instead filled a nested presenter's relay queue until it dropped records, and
        // every drop cost a recovery episode the producer could not see.
        let paced = Instant::now();
        for _ in 0..20 {
            pacer.admit_record();
        }
        assert!(
            paced.elapsed() >= Duration::from_millis(150),
            "twenty records at a hundred per second cannot arrive in {:?}",
            paced.elapsed()
        );
    }

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
