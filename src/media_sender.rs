//! One persistent, bounded media writer. Control stays on the calling coordinator.
use crate::ffmpeg::{EncodedAudioPacket, EncodedPacket};
use std::io;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use vivid_protocol::media::{AudioPacket, VideoPacket};
use vivid_sdk::TrackChannel;

const SEND_TIMEOUT: Duration = Duration::from_secs(30);
enum Payload {
    Video(EncodedPacket),
    Audio(EncodedAudioPacket),
}
struct Job {
    channel: Arc<TrackChannel>,
    epoch: u32,
    id: u64,
    payload: Payload,
    barrier: bool,
}

pub struct MediaSender {
    jobs: Option<mpsc::SyncSender<Job>>,
    results: mpsc::Receiver<io::Result<u64>>,
    worker: Option<thread::JoinHandle<()>>,
    active: Option<Arc<TrackChannel>>,
}
impl MediaSender {
    pub fn new() -> io::Result<Self> {
        let (jobs, input) = mpsc::sync_channel::<Job>(1);
        let (output, results) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("vivi-media".into())
            .spawn(move || {
                while let Ok(job) = input.recv() {
                    let result = match job.payload {
                        Payload::Video(p) => job.channel.send_video(VideoPacket {
                            epoch: job.epoch,
                            packet_id: job.id,
                            pts_us: p.pts_us,
                            dts_us: p.dts_us,
                            duration_us: p.duration_us,
                            key: p.key,
                            data: &p.data,
                        }),
                        Payload::Audio(p) => job.channel.send_audio(AudioPacket {
                            epoch: job.epoch,
                            packet_id: job.id,
                            pts_us: p.pts_us,
                            dts_us: p.dts_us,
                            duration_us: p.duration_us,
                            trim_start_samples: p.trim_start_samples,
                            trim_end_samples: p.trim_end_samples,
                            data: &p.data,
                        }),
                    }
                    .and_then(|sequence| {
                        if job.barrier {
                            job.channel.wait_for_reusable_media_capacity()?;
                        }
                        Ok(sequence)
                    });
                    if output.send(result).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            jobs: Some(jobs),
            results,
            worker: Some(worker),
            active: None,
        })
    }
    pub fn video(
        &mut self,
        channel: Arc<TrackChannel>,
        epoch: u32,
        id: u64,
        packet: &mut EncodedPacket,
        barrier: bool,
    ) -> io::Result<()> {
        let owned = EncodedPacket {
            data: std::mem::take(&mut packet.data),
            key: packet.key,
            pts_us: packet.pts_us,
            dts_us: packet.dts_us,
            duration_us: packet.duration_us,
        };
        self.submit(Job {
            channel,
            epoch,
            id,
            payload: Payload::Video(owned),
            barrier,
        })
    }
    pub fn audio(
        &mut self,
        channel: Arc<TrackChannel>,
        epoch: u32,
        id: u64,
        packet: EncodedAudioPacket,
    ) -> io::Result<()> {
        self.submit(Job {
            channel,
            epoch,
            id,
            payload: Payload::Audio(packet),
            barrier: false,
        })
    }
    fn submit(&mut self, job: Job) -> io::Result<()> {
        if self.active.is_some() {
            return Err(io::Error::other(
                "media writer already has a pending record",
            ));
        }
        self.active = Some(job.channel.clone());
        self.jobs
            .as_ref()
            .ok_or_else(|| io::Error::other("media writer stopped"))?
            .try_send(job)
            .map_err(|_| io::Error::other("media writer unavailable"))
    }
    /// The callback services control while a write waits for flow or transport. True suspends the
    /// media deadline during an intentional pause. An error cancels and consumes this receipt.
    /// A seek callback must retire the channel generation before returning its interruption, so
    /// a presenter cannot interpret the cancelled write as loss of the live track.
    pub fn wait(
        &mut self,
        mut control: impl FnMut() -> io::Result<bool>,
    ) -> io::Result<io::Result<u64>> {
        let mut deadline = Instant::now() + SEND_TIMEOUT;
        loop {
            match control() {
                Ok(true) => deadline = Instant::now() + SEND_TIMEOUT,
                Ok(false) => {}
                Err(error) => {
                    self.cancel_pending();
                    return Err(error);
                }
            }
            match self.results.recv_timeout(Duration::from_millis(5)) {
                Ok(result) => {
                    self.active = None;
                    return Ok(result);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("media writer stopped"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if Instant::now() >= deadline {
                self.cancel_pending();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "media write stalled for 30 seconds; retry playback or check the presenter",
                ));
            }
        }
    }
    fn cancel_pending(&mut self) {
        if let Some(channel) = self.active.take() {
            let _ = channel.close();
            // SDK close interrupts both flow waits and native writes independently of writer locks.
            let _ = self.results.recv();
        }
    }
}
impl Drop for MediaSender {
    fn drop(&mut self) {
        self.cancel_pending();
        self.jobs.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vivid_sdk::testing::{ROOT_SECRET_HEX, Script, TargetKind, TestPresenter};
    use vivid_sdk::{ProducerAuthentication, ProducerConfig, RequestMetadata};
    #[test]
    fn stalled_media_services_control_and_cancellation_joins_the_worker() {
        let script = Script::new();
        script.stall_flow(2);
        let presenter = TestPresenter::start_with(
            TargetKind::Terminal {
                columns: 80,
                rows: 24,
            },
            script,
        )
        .unwrap();
        let mut session = vivid_sdk::Session::connect(ProducerConfig {
            endpoint_control: Some(presenter.endpoint().into()),
            endpoint_realtime: Some(presenter.endpoint().into()),
            endpoint_bulk: Some(presenter.endpoint().into()),
            authentication: ProducerAuthentication::root_hex(ROOT_SECRET_HEX).unwrap(),
            ..Default::default()
        })
        .unwrap();
        session
            .create_surface(
                vivid_sdk::SurfaceDefinition {
                    context_id: session.info().root_context_id,
                    surface_id: 1,
                    semantic_profile: vivid_sdk::GENERIC_CONTENT.into(),
                    coordinate_model: vivid_sdk::CoordinateModel::DesktopLogicalPixels,
                    logical_width: 1,
                    logical_height: 1,
                    scale_numerator: 1,
                    scale_denominator: 1,
                    rotation: 0,
                    descriptor: vivid_sdk::SurfaceDescriptor {
                        role: vivid_sdk::SurfaceRole::Figure,
                        title: String::new(),
                        semantic_content_revision: 0,
                        semantic_availability: 0,
                        locator_hint: String::new(),
                    },
                    policy: 0,
                    profile_parameters: vec![],
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        let configuration = vivid_protocol::track::TrackConfiguration {
            direction: Default::default(),
            context_id: session.info().root_context_id,
            surface_id: 1,
            track_id: 2,
            slot: 2,
            mode: vivid_protocol::track::TrackMode::Timed,
            lane: vivid_protocol::messages::LaneClass::Realtime,
            maximum_record_body: 1024,
            maximum_rate_millihertz: 50_000,
            maximum_encoded_bits_per_second: 512_000,
            maximum_records_per_second: 50,
            maximum_inflight_body_bytes: 4096,
            kind: vivid_protocol::track::KindConfiguration::Audio(vivid_sdk::AudioConfiguration {
                codec: "pcm_s16le".into(),
                packetization: "pcm-packet-v1".into(),
                extradata: vec![],
                sample_rate: 48_000,
                channels: 2,
                channel_mask: 3,
                maximum_access_unit_bytes: 256,
                codec_string: None,
            }),
            target_latency_us: 0,
            maximum_latency_us: 1_000_000,
            retained_pixel_charge: 0,
        };
        let track = session
            .create_track(configuration, &RequestMetadata::default())
            .unwrap();
        let channel = Arc::new(session.open_track_channel(&track).unwrap());
        let packet = || EncodedAudioPacket {
            data: vec![0; 4],
            pts_us: 0,
            dts_us: 0,
            duration_us: 20_000,
            trim_start_samples: 0,
            trim_end_samples: 0,
        };
        let mut sender = MediaSender::new().unwrap();
        sender.audio(channel.clone(), 1, 1, packet()).unwrap();
        sender.wait(|| Ok(false)).unwrap().unwrap();
        sender.audio(channel, 1, 2, packet()).unwrap();
        let started = Instant::now();
        let mut polls = 0;
        let error = sender
            .wait(|| {
                polls += 1;
                if polls == 1 {
                    session.pause(&track)?;
                }
                if polls == 3 {
                    session.play(&track, 0, 1, 1_000_000)?;
                    crate::audio_streamer::retire_audio_for_seek(&mut session, &track)?;
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "seek"));
                }
                Ok(false)
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(track.channel_generation().get(), 2);
        session.flush(&track, 2).unwrap();
        let replacement = session.open_track_channel(&track).unwrap();
        assert_eq!(replacement.generation(), track.channel_generation());
        replacement.close().unwrap();
        drop(sender);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            presenter
                .observed()
                .iter()
                .any(|record| record.record_type == vivid_protocol::messages::PLAY)
        );
    }
}
