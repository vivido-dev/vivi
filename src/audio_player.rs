use std::io;
use std::path::Path;

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
mod platform {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, I24, SampleFormat, SizedSample, Stream, StreamConfig, U24};
    use ringbuf::HeapRb;
    use ringbuf::traits::{Consumer, Producer, Split};

    use super::{Path, alignment_samples, io};
    use crate::ffmpeg::AudioDecoder;

    const RING_BUFFER_SECONDS: usize = 2;
    const PREBUFFER_MILLIS: u64 = 100;
    const POLL_INTERVAL: Duration = Duration::from_millis(2);

    struct SharedState {
        enabled: AtomicBool,
        stop: AtomicBool,
        decode_done: AtomicBool,
        queued_samples: AtomicU64,
        played_samples: AtomicU64,
        gain_bits: AtomicU32,
        error: Mutex<Option<String>>,
    }

    impl SharedState {
        fn new() -> Self {
            Self {
                enabled: AtomicBool::new(false),
                stop: AtomicBool::new(false),
                decode_done: AtomicBool::new(false),
                queued_samples: AtomicU64::new(0),
                played_samples: AtomicU64::new(0),
                gain_bits: AtomicU32::new(1.0_f32.to_bits()),
                error: Mutex::new(None),
            }
        }

        fn set_error(&self, message: impl Into<String>) {
            let mut error = self
                .error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if error.is_none() {
                *error = Some(message.into());
            }
        }

        fn error(&self) -> Option<String> {
            self.error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    struct Ready {
        sample_rate: u32,
        channels: u16,
    }

    pub struct AudioPlayback {
        shared: Arc<SharedState>,
        worker: Option<JoinHandle<()>>,
        prebuffer_samples: u64,
    }

    impl AudioPlayback {
        pub fn open(path: &Path, video_origin_us: Option<i64>) -> io::Result<Self> {
            let path = path.to_path_buf();
            let shared = Arc::new(SharedState::new());
            let worker_shared = shared.clone();
            let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
            let worker = thread::spawn(move || {
                run_worker(path, video_origin_us, worker_shared, ready_sender);
            });
            let ready = match ready_receiver.recv() {
                Ok(Ok(ready)) => ready,
                Ok(Err(error)) => {
                    let _ = worker.join();
                    return Err(error);
                }
                Err(_) => {
                    let _ = worker.join();
                    return Err(io::Error::other(
                        "audio worker stopped before output was ready",
                    ));
                }
            };
            let prebuffer_samples = u64::from(ready.sample_rate)
                .saturating_mul(u64::from(ready.channels))
                .saturating_mul(PREBUFFER_MILLIS)
                / 1_000;
            Ok(Self {
                shared,
                worker: Some(worker),
                prebuffer_samples,
            })
            .and_then(|playback| {
                playback.wait_for_prebuffer()?;
                Ok(playback)
            })
        }

        fn wait_for_prebuffer(&self) -> io::Result<()> {
            while self.shared.queued_samples.load(Ordering::SeqCst) < self.prebuffer_samples
                && !self.shared.decode_done.load(Ordering::SeqCst)
                && !self.shared.stop.load(Ordering::SeqCst)
            {
                if let Some(error) = self.shared.error() {
                    return Err(io::Error::other(error));
                }
                thread::sleep(POLL_INTERVAL);
            }
            if let Some(error) = self.shared.error() {
                return Err(io::Error::other(error));
            }
            Ok(())
        }

        pub fn start(&self) -> io::Result<()> {
            self.wait_for_prebuffer()?;
            self.shared.enabled.store(true, Ordering::SeqCst);
            Ok(())
        }

        pub fn set_volume_percent(&self, percent: u32) {
            let gain = (percent.min(200) as f32 / 100.0).to_bits();
            self.shared.gain_bits.store(gain, Ordering::SeqCst);
        }

        pub fn wait(&mut self) -> io::Result<()> {
            if let Some(worker) = self.worker.take()
                && worker.join().is_err()
            {
                return Err(io::Error::other("audio worker panicked"));
            }
            if let Some(error) = self.shared.error() {
                return Err(io::Error::other(error));
            }
            Ok(())
        }

        pub fn is_finished(&self) -> bool {
            self.worker.as_ref().is_none_or(JoinHandle::is_finished)
        }

        fn stop(&mut self) {
            self.shared.stop.store(true, Ordering::SeqCst);
            self.shared.enabled.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    impl Drop for AudioPlayback {
        fn drop(&mut self) {
            self.stop();
        }
    }

    pub fn prepare_video(path: &Path, video_origin_us: Option<i64>) -> io::Result<AudioPlayback> {
        AudioPlayback::open(path, video_origin_us)
    }

    fn run_worker(
        path: PathBuf,
        video_origin_us: Option<i64>,
        shared: Arc<SharedState>,
        ready: mpsc::SyncSender<io::Result<Ready>>,
    ) {
        if let Err(error) = run_worker_inner(&path, video_origin_us, &shared, &ready) {
            let _ = ready.try_send(Err(io::Error::new(error.kind(), error.to_string())));
            shared.set_error(error.to_string());
        }
        shared.decode_done.store(true, Ordering::SeqCst);
    }

    fn run_worker_inner(
        path: &Path,
        video_origin_us: Option<i64>,
        shared: &Arc<SharedState>,
        ready: &mpsc::SyncSender<io::Result<Ready>>,
    ) -> io::Result<()> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no default audio output device")
        })?;
        let supported = device.default_output_config().map_err(|error| {
            io::Error::other(format!("could not query the default audio output: {error}"))
        })?;
        let sample_format = supported.sample_format();
        let output_config: StreamConfig = supported.into();
        let sample_rate = output_config.sample_rate;
        let channels = output_config.channels;
        let mut decoder = AudioDecoder::open(path, sample_rate, channels)?;

        let capacity = sample_rate as usize * channels as usize * RING_BUFFER_SECONDS;
        let ring = HeapRb::<f32>::new(capacity.max(1));
        let (mut producer, consumer) = ring.split();
        let callback_shared = shared.clone();
        let stream = match sample_format {
            SampleFormat::I8 => {
                build_output_stream::<i8, _>(&device, &output_config, consumer, callback_shared)
            }
            SampleFormat::I16 => {
                build_output_stream::<i16, _>(&device, &output_config, consumer, callback_shared)
            }
            SampleFormat::I24 => {
                build_output_stream::<I24, _>(&device, &output_config, consumer, callback_shared)
            }
            SampleFormat::I32 => {
                build_output_stream::<i32, _>(&device, &output_config, consumer, callback_shared)
            }
            SampleFormat::I64 => {
                build_output_stream::<i64, _>(&device, &output_config, consumer, callback_shared)
            }
            SampleFormat::U8 => {
                build_output_stream::<u8, _>(&device, &output_config, consumer, callback_shared)
            }
            SampleFormat::U16 => {
                build_output_stream::<u16, _>(&device, &output_config, consumer, callback_shared)
            }
            SampleFormat::U24 => {
                build_output_stream::<U24, _>(&device, &output_config, consumer, callback_shared)
            }
            SampleFormat::U32 => {
                build_output_stream::<u32, _>(&device, &output_config, consumer, callback_shared)
            }
            SampleFormat::U64 => {
                build_output_stream::<u64, _>(&device, &output_config, consumer, callback_shared)
            }
            SampleFormat::F32 => {
                build_output_stream::<f32, _>(&device, &output_config, consumer, callback_shared)
            }
            SampleFormat::F64 => {
                build_output_stream::<f64, _>(&device, &output_config, consumer, callback_shared)
            }
            unsupported => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("audio output uses unsupported sample format {unsupported}"),
            )),
        }?;
        stream
            .play()
            .map_err(|error| io::Error::other(format!("could not start audio output: {error}")))?;

        ready
            .send(Ok(Ready {
                sample_rate,
                channels,
            }))
            .map_err(|_| io::Error::other("audio playback was cancelled during setup"))?;

        let mut aligned = false;
        let mut trim_samples = 0_u64;
        while !shared.stop.load(Ordering::SeqCst) {
            let Some(mut frame) = decoder.next_frame()? else {
                break;
            };
            if !aligned {
                let offset = alignment_samples(
                    video_origin_us,
                    decoder.first_pts_us(),
                    sample_rate,
                    channels,
                );
                if offset > 0 {
                    push_repeated(&mut producer, 0.0, offset as u64, shared)?;
                } else if offset < 0 {
                    trim_samples = offset.unsigned_abs();
                }
                aligned = true;
            }
            if trim_samples != 0 {
                let trim = usize::try_from(trim_samples)
                    .unwrap_or(usize::MAX)
                    .min(frame.samples.len());
                frame.samples.drain(..trim);
                trim_samples -= trim as u64;
            }
            push_samples(&mut producer, &frame.samples, shared)?;
        }
        shared.decode_done.store(true, Ordering::SeqCst);

        while !shared.stop.load(Ordering::SeqCst) {
            if shared.error().is_some() {
                break;
            }
            let queued = shared.queued_samples.load(Ordering::SeqCst);
            let played = shared.played_samples.load(Ordering::SeqCst);
            if shared.enabled.load(Ordering::SeqCst) && played >= queued {
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }
        drop(stream);
        Ok(())
    }

    fn build_output_stream<T, C>(
        device: &cpal::Device,
        config: &StreamConfig,
        mut consumer: C,
        shared: Arc<SharedState>,
    ) -> io::Result<Stream>
    where
        T: SizedSample + FromSample<f32>,
        C: Consumer<Item = f32> + Send + 'static,
    {
        let error_shared = shared.clone();
        device
            .build_output_stream(
                *config,
                move |output: &mut [T], _| {
                    if !shared.enabled.load(Ordering::SeqCst) {
                        output.fill_with(|| T::from_sample(0.0));
                        return;
                    }
                    let mut played = 0_u64;
                    let gain = f32::from_bits(shared.gain_bits.load(Ordering::SeqCst));
                    for sample in output {
                        if let Some(value) = consumer.try_pop() {
                            *sample = T::from_sample((value * gain).clamp(-1.0, 1.0));
                            played += 1;
                        } else {
                            *sample = T::from_sample(0.0);
                        }
                    }
                    shared.played_samples.fetch_add(played, Ordering::SeqCst);
                },
                move |error| {
                    error_shared.set_error(format!("audio output stream error: {error}"));
                },
                None,
            )
            .map_err(|error| io::Error::other(format!("could not build audio output: {error}")))
    }

    fn push_repeated<P: Producer<Item = f32>>(
        producer: &mut P,
        value: f32,
        count: u64,
        shared: &SharedState,
    ) -> io::Result<()> {
        for _ in 0..count {
            if shared.stop.load(Ordering::SeqCst) {
                break;
            }
            push_one(producer, value, shared)?;
        }
        Ok(())
    }

    fn push_samples<P: Producer<Item = f32>>(
        producer: &mut P,
        samples: &[f32],
        shared: &SharedState,
    ) -> io::Result<()> {
        for &sample in samples {
            if shared.stop.load(Ordering::SeqCst) {
                break;
            }
            push_one(producer, sample, shared)?;
        }
        Ok(())
    }

    fn push_one<P: Producer<Item = f32>>(
        producer: &mut P,
        mut sample: f32,
        shared: &SharedState,
    ) -> io::Result<()> {
        loop {
            if shared.stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            if let Some(error) = shared.error() {
                return Err(io::Error::other(error));
            }
            match producer.try_push(sample) {
                Ok(()) => {
                    shared.queued_samples.fetch_add(1, Ordering::SeqCst);
                    return Ok(());
                }
                Err(returned) => {
                    sample = returned;
                    thread::sleep(Duration::from_micros(500));
                }
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub use platform::{AudioPlayback, prepare_video};

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod platform_stub {
    use super::{Path, io};

    pub struct AudioPlayback;

    impl AudioPlayback {
        pub fn start(&self) -> io::Result<()> {
            Ok(())
        }

        pub fn wait(&mut self) -> io::Result<()> {
            Ok(())
        }

        pub fn is_finished(&self) -> bool {
            true
        }

        pub fn set_volume_percent(&self, _percent: u32) {}
    }

    pub fn prepare_video(_path: &Path, _video_origin_us: Option<i64>) -> io::Result<AudioPlayback> {
        Ok(AudioPlayback)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub use platform_stub::{AudioPlayback, prepare_video};

pub fn play(config: &crate::cli::Config, path: &Path) -> io::Result<()> {
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (config, path);
        return Err(unsupported_platform_error());
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    {
        use std::thread;
        use std::time::Duration;

        use crate::ffmpeg::AudioDemuxer;
        use crate::playback_ui::{Command, PlaybackUi};

        let info = AudioDemuxer::inspect(path)?;
        let ui = PlaybackUi::enter(config, path, info.duration_us, true, true)?;
        let mut playback = AudioPlayback::open(path, None)?;
        playback.start()?;
        let started = std::time::Instant::now();
        let mut volume_percent = 100_u32;
        while !playback.is_finished() {
            if let Some(ui) = ui.as_ref() {
                ui.set_position_us(
                    u64::try_from(started.elapsed().as_micros())
                        .unwrap_or(u64::MAX)
                        .min(info.duration_us.unwrap_or(u64::MAX)),
                );
                while let Some(command) = ui.try_command() {
                    match command {
                        Command::Quit => return Ok(()),
                        Command::VolumeBy(delta) => {
                            volume_percent = (volume_percent as i32 + delta).clamp(0, 200) as u32;
                            playback.set_volume_percent(volume_percent);
                            ui.set_volume_percent(Some(volume_percent));
                            ui.set_message(format!("Volume {volume_percent}%"));
                        }
                        Command::SeekBy(_) | Command::SeekTo(_) => {
                            ui.set_message("Seek requires presenter audio");
                        }
                        Command::Resize(_) => {}
                    }
                }
                ui.redraw()?;
            }
            thread::sleep(Duration::from_millis(50));
        }
        playback.wait()
    }
}

#[cfg(any(not(any(target_os = "macos", target_os = "linux", windows)), test))]
fn unsupported_platform_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "audio playback is currently supported only on macOS, Linux, and Windows",
    )
}

fn alignment_samples(
    video_origin_us: Option<i64>,
    audio_origin_us: Option<i64>,
    sample_rate: u32,
    channels: u16,
) -> i64 {
    let (Some(video), Some(audio)) = (video_origin_us, audio_origin_us) else {
        return 0;
    };
    let samples = i128::from(audio.saturating_sub(video))
        .saturating_mul(i128::from(sample_rate))
        .saturating_mul(i128::from(channels))
        / 1_000_000;
    samples.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// Synthesizes a small in-memory PCM WAV byte vector for tests. Pure byte assembly — no
/// platform dependency, no audio device.
#[cfg(test)]
pub(crate) fn pcm_wav() -> Vec<u8> {
    let sample_rate = 8_000_u32;
    let sample_count = 80_u32;
    let data_bytes = sample_count * 2;
    let mut wav = Vec::with_capacity((44 + data_bytes) as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    for index in 0..sample_count {
        let sample = if index % 2 == 0 {
            1_000_i16
        } else {
            -1_000_i16
        };
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment_inserts_interleaved_silence_for_late_audio() {
        assert_eq!(alignment_samples(Some(0), Some(250_000), 48_000, 2), 24_000);
    }

    #[test]
    fn alignment_trims_interleaved_samples_for_early_audio() {
        assert_eq!(
            alignment_samples(Some(500_000), Some(0), 48_000, 2),
            -48_000
        );
    }

    #[test]
    fn alignment_starts_together_without_both_timestamps() {
        assert_eq!(alignment_samples(None, Some(10), 48_000, 2), 0);
        assert_eq!(alignment_samples(Some(10), None, 48_000, 2), 0);
    }

    #[test]
    fn unsupported_platform_error_is_explicit() {
        let error = unsupported_platform_error();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("macOS, Linux, and Windows"));
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn ffmpeg_decodes_generated_pcm_wav_without_an_output_device() {
        use std::fs;
        use std::sync::atomic::{AtomicU64, Ordering};

        use crate::ffmpeg::AudioDecoder;

        static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vivi-audio-decoder-{}-{sequence}.wav",
            std::process::id()
        ));
        fs::write(&path, pcm_wav()).unwrap();
        let result = (|| {
            let mut decoder = AudioDecoder::open(&path, 48_000, 2)?;
            let mut samples = 0_usize;
            while let Some(frame) = decoder.next_frame()? {
                samples += frame.samples.len();
            }
            io::Result::Ok(samples)
        })();
        let _ = fs::remove_file(&path);
        assert!(result.unwrap() > 0);
    }
}
