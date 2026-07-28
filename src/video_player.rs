use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use crate::audio_player;
use crate::cli::Config;
use crate::client::{
    KeyframeRequest, MediaSender, PresenterError, SourceWaitHandle, VividClient, WaitSource,
};
use crate::ffmpeg::{AudioDemuxer, EncodedMediaPacket, EncodedPacket, VideoDemuxer};
use crate::protocol::media::{AudioPacket, VideoPacket};
use crate::protocol::wire::ConnectionKind;
use crate::terminal_geometry::{TerminalGeometry, cells_for_pixels, reserve_rows};

const FIT_MARGIN_COLS: u16 = 4;
const FIT_MARGIN_ROWS: u16 = 2;
const INITIAL_BUFFER_US: u64 = 33_000;
const AUDIO_PREBUFFER_US: u64 = 100_000;
const PLAYBACK_START_TIMEOUT: Duration = Duration::from_secs(30);
const PLAYBACK_COMPLETION_GRACE: Duration = Duration::from_secs(30);
const SOURCE_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackPhase {
    Pending,
    Streaming,
    IngressClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryAction {
    None,
    Flush,
    RebasePlayback,
}

impl PlaybackPhase {
    fn admitted(self) -> bool {
        self != Self::Pending
    }

    fn may_join_presenter_wait(self) -> bool {
        self == Self::IngressClosed
    }
}

struct PlaybackState {
    packet_id: u64,
    encoded_bytes: u64,
    playback_phase: PlaybackPhase,
    playback_wait: Option<SourceWaitHandle>,
    audio_started: bool,
    first_pts_us: Option<i64>,
    last_pts_us: Option<i64>,
    epoch: u32,
    awaiting_keyframe: bool,
    recovery_requires_flush: bool,
    recovery_rebases_playback: bool,
    audio_buffered_us: u64,
    audio_horizon_us: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AudioStreamSnapshot {
    buffered_us: u64,
    horizon_us: Option<i64>,
    finished: bool,
    failed: bool,
}

#[derive(Default)]
struct AudioStreamProgress {
    state: Mutex<AudioStreamSnapshot>,
    changed: Condvar,
}

impl AudioStreamProgress {
    fn observe(&self, pts_us: i64, duration_us: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.buffered_us = state.buffered_us.saturating_add(duration_us);
        let duration_us = i64::try_from(duration_us).unwrap_or(i64::MAX);
        if pts_us != i64::MIN {
            let end = pts_us.saturating_add(duration_us);
            state.horizon_us = Some(state.horizon_us.map_or(end, |last| last.max(end)));
        } else if let Some(horizon) = state.horizon_us.as_mut() {
            *horizon = horizon.saturating_add(duration_us);
        }
        self.changed.notify_all();
    }

    fn finish(&self, failed: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.finished = true;
        state.failed = failed;
        self.changed.notify_all();
    }

    fn snapshot(&self) -> AudioStreamSnapshot {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn wait_for_prebuffer(&self, minimum_us: u64, timeout: Duration) -> AudioStreamSnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.buffered_us < minimum_us && !state.finished
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *state
    }
}

struct AudioStreamOutcome {
    sender: MediaSender,
    packet_id: u64,
    error: Option<io::Error>,
}

struct VividAudioStream {
    source_id: u64,
    progress: Arc<AudioStreamProgress>,
    worker: thread::JoinHandle<AudioStreamOutcome>,
}

impl VividAudioStream {
    fn start(path: &Path, source_id: u64, sender: MediaSender, epoch: u32) -> Self {
        let progress = Arc::new(AudioStreamProgress::default());
        let worker_progress = progress.clone();
        let path = path.to_path_buf();
        let worker = thread::spawn(move || {
            let mut sender = sender;
            let mut packet_id = 0_u64;
            let result = (|| -> io::Result<()> {
                let mut demuxer = AudioDemuxer::open(&path)?;
                while let Some(packet) = demuxer.next_packet()? {
                    if packet.data.is_empty() {
                        continue;
                    }
                    packet_id = packet_id
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("audio packet ID space exhausted"))?;
                    sender.send_audio(AudioPacket {
                        epoch,
                        packet_id,
                        pts_us: packet.pts_us,
                        dts_us: packet.dts_us,
                        duration_us: packet.duration_us,
                        trim_start_samples: packet.trim_start_samples,
                        trim_end_samples: packet.trim_end_samples,
                        data: &packet.data,
                    })?;
                    worker_progress.observe(packet.pts_us, packet.duration_us);
                }
                Ok(())
            })();
            worker_progress.finish(result.is_err());
            AudioStreamOutcome {
                sender,
                packet_id,
                error: result.err(),
            }
        });
        Self {
            source_id,
            progress,
            worker,
        }
    }

    fn apply_progress(&self, state: &mut PlaybackState) -> AudioStreamSnapshot {
        let progress = self.progress.snapshot();
        state.audio_buffered_us = progress.buffered_us;
        state.audio_horizon_us = progress.horizon_us;
        progress
    }

    fn wait_for_prebuffer(&self, state: &mut PlaybackState) -> AudioStreamSnapshot {
        let progress = self
            .progress
            .wait_for_prebuffer(AUDIO_PREBUFFER_US, PLAYBACK_START_TIMEOUT);
        state.audio_buffered_us = progress.buffered_us;
        state.audio_horizon_us = progress.horizon_us;
        progress
    }

    fn join(self) -> io::Result<AudioStreamOutcome> {
        self.worker
            .join()
            .map_err(|_| io::Error::other("audio media worker panicked"))
    }
}

impl PlaybackState {
    fn new() -> Self {
        Self {
            packet_id: 0,
            encoded_bytes: 0,
            playback_phase: PlaybackPhase::Pending,
            playback_wait: None,
            audio_started: false,
            first_pts_us: None,
            last_pts_us: None,
            epoch: 1,
            awaiting_keyframe: false,
            recovery_requires_flush: false,
            recovery_rebases_playback: false,
            audio_buffered_us: 0,
            audio_horizon_us: None,
        }
    }

    fn note_keyframe_request(&mut self, request: KeyframeRequest) {
        if request.minimum_epoch > self.epoch {
            self.epoch = request.minimum_epoch;
            self.recovery_requires_flush = true;
        } else if request.reason == crate::protocol::messages::KEYFRAME_REASON_INITIAL
            && self.playback_phase.admitted()
        {
            // The bridge uses INITIAL when a tab switch recreated the outer decoder. This is not
            // transport loss: the new source has no useful PLAY clock. Keep the current epoch, but
            // reissue PLAY at the recovery keyframe before forwarding that packet.
            self.recovery_rebases_playback = true;
        }
        self.awaiting_keyframe = true;
    }

    fn take_recovery_action(&mut self) -> RecoveryAction {
        self.awaiting_keyframe = false;
        if std::mem::take(&mut self.recovery_requires_flush) {
            self.recovery_rebases_playback = false;
            RecoveryAction::Flush
        } else if std::mem::take(&mut self.recovery_rebases_playback) {
            RecoveryAction::RebasePlayback
        } else {
            RecoveryAction::None
        }
    }

    /// FLUSH invalidates the presenter's PLAY state, so playback must restart. The restarted
    /// PLAY has to begin at the recovery keyframe's timeline position: reusing the original
    /// stream start would schedule every resumed frame `already-played` seconds of wall time
    /// into the future, leaving the source blank and its linked audio silent.
    fn begin_recovery_restart(&mut self) {
        self.playback_phase = PlaybackPhase::Pending;
        self.playback_wait = None;
        self.first_pts_us = None;
        self.audio_buffered_us = 0;
        self.audio_horizon_us = None;
    }

    #[cfg(test)]
    fn observe_audio_packet(&mut self, pts_us: i64, duration_us: u64) {
        self.audio_buffered_us = self.audio_buffered_us.saturating_add(duration_us);
        let duration_us = i64::try_from(duration_us).unwrap_or(i64::MAX);
        if pts_us != i64::MIN {
            let end = pts_us.saturating_add(duration_us);
            self.audio_horizon_us = Some(self.audio_horizon_us.map_or(end, |last| last.max(end)));
        } else if let Some(horizon) = self.audio_horizon_us.as_mut() {
            *horizon = horizon.saturating_add(duration_us);
        }
    }

    /// Resume at the packet that will be submitted after visibility returns. Reusing the
    /// stream's first PTS restarts the presenter clock at the beginning while ingress is already
    /// much later, which leaves resumed video scheduled in the future and lets linked audio
    /// drain against the stale clock.
    fn visibility_resume_pts(&self, packet_pts_us: i64) -> i64 {
        if packet_pts_us != i64::MIN {
            packet_pts_us
        } else {
            self.last_pts_us.or(self.first_pts_us).unwrap_or(0)
        }
    }
}

/// The initial linked-A/V start can only begin on a keyframe, so undecodable leading delta
/// frames are discarded. A restart after keyframe recovery has already submitted the new
/// epoch's keyframe, so the queued delta frames that follow it are decodable and must be kept.
fn linked_start_ready(state: &PlaybackState, pending_video: &mut VecDeque<EncodedPacket>) -> bool {
    if state.packet_id > 0 {
        return true;
    }
    while pending_video.front().is_some_and(|packet| !packet.key) {
        pending_video.pop_front();
    }
    pending_video.front().is_some_and(|packet| packet.key)
}

fn start_playback(
    client: &mut VividClient,
    source_id: u64,
    minimum_buffer_us: u64,
    audio: &mut Option<audio_player::AudioPlayback>,
    state: &mut PlaybackState,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    state.playback_wait = admit_playback(
        client,
        source_id,
        state.first_pts_us.unwrap_or(0),
        minimum_buffer_us,
    )?;
    state.playback_phase = PlaybackPhase::Streaming;
    if !state.audio_started
        && let Some(playback) = audio.as_ref()
    {
        match playback.start() {
            Ok(()) => state.audio_started = true,
            Err(error) => {
                client.verbose(format_args!(
                    "audio disabled for {} after output start failed: {error}",
                    path.display()
                ));
                *audio = None;
            }
        }
    }
    Ok(())
}

fn admit_playback(
    client: &mut VividClient,
    source_id: u64,
    start_pts_us: i64,
    minimum_buffer_us: u64,
) -> io::Result<Option<SourceWaitHandle>> {
    let wait = client
        .supports(crate::protocol::messages::FEATURE_OBSERVABILITY_CORE_V1)
        .then(|| {
            client.begin_wait_source(WaitSource {
                source_id,
                condition: crate::protocol::messages::WAIT_PLAYBACK_STARTED,
                value: None,
                timeout_us: u64::try_from(PLAYBACK_START_TIMEOUT.as_micros()).unwrap(),
            })
        })
        .transpose()?;
    client.play_at(source_id, start_pts_us, minimum_buffer_us)?;
    Ok(wait)
}

fn is_presenter_timeout(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|error| error.downcast_ref::<PresenterError>())
        .is_some_and(|error| error.code == crate::protocol::messages::ERROR_TIMEOUT)
}

fn wait_in_source_timeout_slices(
    total_timeout: Duration,
    mut wait: impl FnMut(Duration) -> io::Result<()>,
) -> io::Result<()> {
    let mut remaining = total_timeout;
    while !remaining.is_zero() {
        let timeout = remaining.min(SOURCE_WAIT_TIMEOUT);
        match wait(timeout) {
            Ok(()) => return Ok(()),
            Err(error) if is_presenter_timeout(&error) => {
                remaining = remaining.saturating_sub(timeout);
                if remaining.is_zero() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn wait_for_playback_end(
    client: &mut VividClient,
    source_id: u64,
    total_timeout: Duration,
) -> io::Result<()> {
    wait_in_source_timeout_slices(total_timeout, |timeout| {
        client
            .wait_source(WaitSource {
                source_id,
                condition: crate::protocol::messages::WAIT_PLAYBACK_ENDED,
                value: None,
                timeout_us: u64::try_from(timeout.as_micros())
                    .expect("bounded source wait timeout fits in u64"),
            })
            .map(|_| ())
    })
}

struct VideoSubmitter<'a> {
    client: &'a mut VividClient,
    path: &'a Path,
    source_id: u64,
    sender: &'a mut MediaSender,
    audio: &'a mut Option<audio_player::AudioPlayback>,
}

impl VideoSubmitter<'_> {
    fn submit(
        &mut self,
        state: &mut PlaybackState,
        packet: EncodedPacket,
        start_after_packet: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            self.client
                .apply_pending_source_events(self.sender.source_mut())?;
            if let Some(request) = self.sender.source_mut().take_keyframe_request_detailed() {
                state.note_keyframe_request(request);
            }
            if state.awaiting_keyframe && !packet.key {
                return Ok(());
            }
            let recovery_action = if state.awaiting_keyframe {
                state.take_recovery_action()
            } else {
                RecoveryAction::None
            };
            if recovery_action == RecoveryAction::Flush {
                self.client.flush(self.source_id, state.epoch)?;
                state.begin_recovery_restart();
            }
            let mut rebase_playback = recovery_action == RecoveryAction::RebasePlayback;
            if !self.sender.source().is_visible() {
                if state.audio_started
                    && let Some(playback) = self.audio.as_ref()
                {
                    playback.pause();
                }
                self.client.pause(self.source_id)?;
                self.client.wait_until_visible(self.sender.source_mut())?;
                state.playback_wait = admit_playback(
                    self.client,
                    self.source_id,
                    state.visibility_resume_pts(packet.pts_us),
                    INITIAL_BUFFER_US,
                )?;
                rebase_playback = false;
                if state.audio_started
                    && let Some(playback) = self.audio.as_ref()
                {
                    playback.resume();
                }
            }
            if rebase_playback {
                state.playback_wait = admit_playback(
                    self.client,
                    self.source_id,
                    state.visibility_resume_pts(packet.pts_us),
                    INITIAL_BUFFER_US,
                )?;
            }
            if packet.data.is_empty() {
                return Ok(());
            }
            let packet_id = state
                .packet_id
                .checked_add(1)
                .ok_or_else(|| io::Error::other("video packet ID space exhausted"))?;
            let result = self.sender.send_video(VideoPacket {
                epoch: state.epoch,
                packet_id,
                pts_us: packet.pts_us,
                dts_us: packet.dts_us,
                duration_us: 0,
                key: packet.key,
                data: &packet.data,
            });
            match result {
                Ok(()) => {
                    state.packet_id = packet_id;
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        state.encoded_bytes = state.encoded_bytes.saturating_add(packet.data.len() as u64);
        if packet.pts_us != i64::MIN {
            state.first_pts_us.get_or_insert(packet.pts_us);
            state.last_pts_us = Some(
                state
                    .last_pts_us
                    .map_or(packet.pts_us, |last: i64| last.max(packet.pts_us)),
            );
        }

        if !state.playback_phase.admitted() && start_after_packet {
            start_playback(
                self.client,
                self.source_id,
                INITIAL_BUFFER_US,
                self.audio,
                state,
                self.path,
            )?;
        }
        if state.packet_id.is_multiple_of(120) {
            self.client.verbose(format_args!(
                "video source {}: sent {} packets / {} bytes",
                self.source_id, state.packet_id, state.encoded_bytes
            ));
        }
        Ok(())
    }
}

pub fn play(
    config: &Config,
    client: &mut VividClient,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let info = VideoDemuxer::inspect(path)?;
    if info.colorimetry_inferred {
        client.verbose(format_args!(
            "video {}: inferred missing colorimetry as primaries={}, transfer={}, matrix={}, range={}",
            path.display(),
            info.color_primaries,
            info.transfer,
            info.matrix,
            info.range
        ));
    }
    let mut demuxer = VideoDemuxer::open(path)?;
    let vivid_audio_available = info.audio.is_some()
        && client.supports(crate::protocol::messages::FEATURE_AUDIO_ACCESS_UNIT_V1)
        && !config.no_wait;
    let remote_session = std::env::var_os("VIVID_REMOTE").is_some();
    let mut audio: Option<audio_player::AudioPlayback> = if info.has_audio
        && !vivid_audio_available
        && !config.no_wait
        && !config.is_dry_run()
        && !remote_session
    {
        match audio_player::prepare_video(path, info.first_pts_us) {
            Ok(playback) => Some(playback),
            Err(error) => {
                client.verbose(format_args!(
                    "audio disabled for {} after local output preparation failed: {error}",
                    path.display()
                ));
                None
            }
        }
    } else {
        None
    };
    let (columns, rows) = display_size(
        info.width,
        info.height,
        info.sar_num,
        info.sar_den,
        config.zoom,
        TerminalGeometry::settled_presenter(client),
    );

    let source_id = client.allocate_id()?;
    let node_id = client.allocate_id()?;
    let audio_id = vivid_audio_available
        .then(|| client.allocate_id())
        .transpose()?;
    let (source, presenter_audio) = if let Some(audio_id) = audio_id {
        let (video, audio) = client.create_linked_av_sources(
            source_id,
            &info,
            audio_id,
            info.audio.as_ref().unwrap(),
        )?;
        (video, Some((audio_id, audio)))
    } else {
        (client.create_video_source(source_id, &info)?, None)
    };
    let mut state = PlaybackState::new();
    let mut vivid_audio = if let Some((audio_id, presenter_audio)) = presenter_audio {
        match presenter_audio {
            Ok(audio_source) => {
                let audio_sender = client.open_media_sender(audio_source, ConnectionKind::Audio)?;
                Some(VividAudioStream::start(
                    path,
                    audio_id,
                    audio_sender,
                    state.epoch,
                ))
            }
            Err(error) => {
                if !remote_session && !config.is_dry_run() {
                    match audio_player::prepare_video(path, info.first_pts_us) {
                        Ok(playback) => audio = Some(playback),
                        Err(fallback_error) => client.verbose(format_args!(
                            "audio disabled for {} after presenter output failed ({error}) and local output preparation failed ({fallback_error})",
                            path.display(),
                        )),
                    }
                } else {
                    client.verbose(format_args!(
                        "audio disabled for {} after presenter output failed: {error}",
                        path.display()
                    ));
                }
                None
            }
        }
    } else {
        if info.has_audio && remote_session && !config.no_wait {
            client.verbose(format_args!(
                "audio disabled for {} because the remote presenter lacks audio support",
                path.display()
            ));
        }
        None
    };
    let anchor_id = client.create_text_anchor()?;
    client.place_source(source_id, node_id, anchor_id, columns, rows)?;
    if !config.is_dry_run() {
        reserve_rows(rows)?;
    }
    let mut sender = client.open_media_sender(source, ConnectionKind::Video)?;

    client.verbose(format_args!(
        "video {}: codec={} packetization={} {}x{} -> {columns}x{rows} cells",
        path.display(),
        info.codec,
        info.packetization,
        info.width,
        info.height
    ));

    // The media channels are independent, so preserve each stream's decode order rather than the
    // container's cross-stream packet order. Some MP4s front-load enough video to fill the video
    // socket before their first audio packet. The linked audio worker uses an independent demuxer
    // and sender so backpressure on either browser decoder cannot starve the other stream.
    let mut pending_video = VecDeque::new();
    while let Some(media_packet) = demuxer.next_media_packet()? {
        match media_packet {
            // A separate AudioDemuxer feeds the audio media connection. Reading past the
            // container's audio packets here keeps the video demuxer source-specific too.
            EncodedMediaPacket::Audio(_) => {}
            EncodedMediaPacket::Video(packet) => {
                if vivid_audio.is_some() {
                    pending_video.push_back(packet);
                } else {
                    VideoSubmitter {
                        client,
                        path,
                        source_id,
                        sender: &mut sender,
                        audio: &mut audio,
                    }
                    .submit(&mut state, packet, true)?;
                }
            }
        }

        if vivid_audio
            .as_ref()
            .is_some_and(|stream| stream.progress.snapshot().failed)
        {
            let stream = vivid_audio.take().expect("failed audio stream exists");
            let source_id = stream.source_id;
            match stream.join() {
                Ok(outcome) => {
                    client.verbose(format_args!(
                        "audio source {source_id} disabled after {} packets: {}",
                        outcome.packet_id,
                        outcome.error.map_or_else(
                            || "media worker stopped".into(),
                            |error| error.to_string()
                        )
                    ));
                }
                Err(error) => client.verbose(format_args!(
                    "audio source {source_id} disabled after media worker failure: {error}"
                )),
            }
        }

        if let Some(stream) = vivid_audio.as_ref() {
            if !state.playback_phase.admitted() && linked_start_ready(&state, &mut pending_video) {
                let progress = stream.wait_for_prebuffer(&mut state);
                if !progress.failed {
                    start_playback(
                        client,
                        source_id,
                        progress.buffered_us.min(AUDIO_PREBUFFER_US),
                        &mut audio,
                        &mut state,
                        path,
                    )?;
                }
            } else {
                stream.apply_progress(&mut state);
            }
            while state.playback_phase.admitted() && !pending_video.is_empty() {
                let packet = pending_video
                    .pop_front()
                    .expect("pending video packet exists");
                VideoSubmitter {
                    client,
                    path,
                    source_id,
                    sender: &mut sender,
                    audio: &mut audio,
                }
                .submit(&mut state, packet, false)?;
            }
        } else {
            while let Some(packet) = pending_video.pop_front() {
                VideoSubmitter {
                    client,
                    path,
                    source_id,
                    sender: &mut sender,
                    audio: &mut audio,
                }
                .submit(&mut state, packet, true)?;
            }
        }
    }

    if state.packet_id == 0 && !pending_video.is_empty() && !state.playback_phase.admitted() {
        start_playback(
            client,
            source_id,
            state.audio_buffered_us,
            &mut audio,
            &mut state,
            path,
        )?;
    }
    while let Some(packet) = pending_video.pop_front() {
        VideoSubmitter {
            client,
            path,
            source_id,
            sender: &mut sender,
            audio: &mut audio,
        }
        .submit(&mut state, packet, true)?;
    }

    if state.packet_id == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "video has no packets").into());
    }
    if !state.playback_phase.admitted() {
        start_playback(
            client,
            source_id,
            state.audio_buffered_us,
            &mut audio,
            &mut state,
            path,
        )?;
    }
    // The audio media connection is independent and may still be submitting access units after
    // the video demuxer reaches EOF. Join it and apply its source-scoped media-order barrier before
    // closing video ingress, so no final audio record can race a linked video EOS.
    let mut audio_to_drain = None;
    if let Some(stream) = vivid_audio.take() {
        let audio_id = stream.source_id;
        match stream.join() {
            Ok(outcome) => {
                if let Some(error) = outcome.error {
                    client.verbose(format_args!(
                        "video {} completed, but presenter audio stopped after {} packets: {error}",
                        path.display(),
                        outcome.packet_id
                    ));
                } else {
                    match client.eos_sender(&outcome.sender, state.epoch) {
                        Ok(()) => audio_to_drain = Some(audio_id),
                        Err(error) => client.verbose(format_args!(
                            "video {} completed, but presenter audio EOS failed: {error}",
                            path.display()
                        )),
                    }
                }
            }
            Err(error) => client.verbose(format_args!(
                "video {} completed, but presenter audio worker failed: {error}",
                path.display()
            )),
        }
    }
    client.eos_sender(&sender, state.epoch)?;
    state.playback_phase = PlaybackPhase::IngressClosed;
    debug_assert!(state.playback_phase.may_join_presenter_wait());
    if let Some(mut wait) = state.playback_wait.take() {
        wait.wait()?;
    }
    if let Some(audio_id) = audio_to_drain
        && let Err(error) = client.drain(audio_id)
    {
        client.verbose(format_args!(
            "video {} completed, but presenter audio drain failed: {error}",
            path.display()
        ));
    }
    if !config.no_wait
        && client.supports(crate::protocol::messages::FEATURE_OBSERVABILITY_CORE_V1)
        && let (Some(first_pts), Some(last_pts)) = (state.first_pts_us, state.last_pts_us)
    {
        let timeline_us = last_pts.saturating_sub(first_pts).max(0) as u64;
        let timeout = Duration::from_micros(timeline_us).saturating_add(PLAYBACK_COMPLETION_GRACE);
        client.verbose(format_args!(
            "waiting for presenter playback-ended milestone"
        ));
        wait_for_playback_end(client, source_id, timeout)?;
    }
    if state.audio_started
        && let Some(playback) = audio.as_mut()
        && let Err(error) = playback.wait()
    {
        client.verbose(format_args!(
            "video {} completed, but local audio playback failed: {error}",
            path.display()
        ));
    }
    client.verbose(format_args!(
        "video source {source_id}: EOS after {} packets / {} bytes",
        state.packet_id, state.encoded_bytes
    ));
    Ok(())
}

fn display_size(
    width: u32,
    height: u32,
    sar_num: u32,
    sar_den: u32,
    zoom: f32,
    geometry: TerminalGeometry,
) -> (u32, u32) {
    let sample_aspect_ratio = f64::from(sar_num) / f64::from(sar_den.max(1));
    let desired_width = (width as f64 * sample_aspect_ratio * f64::from(zoom))
        .round()
        .max(1.0);
    let desired_height = (height as f64 * f64::from(zoom)).round().max(1.0);
    let maximum_width = f64::from(geometry.drawable_width_px(FIT_MARGIN_COLS));
    let maximum_height = f64::from(geometry.drawable_height_px(FIT_MARGIN_ROWS));
    let scale = (maximum_width / desired_width)
        .min(maximum_height / desired_height)
        .min(1.0);
    let target_width = (desired_width * scale).round().clamp(1.0, maximum_width) as u32;
    let target_height = (desired_height * scale).round().clamp(1.0, maximum_height) as u32;
    (
        cells_for_pixels(target_width, geometry.cell_width_px),
        cells_for_pixels(target_height, geometry.cell_height_px),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presenter_error(code: u64) -> io::Error {
        io::Error::other(PresenterError {
            code,
            request_id: 1,
            fatal: false,
            detail: crate::protocol::messages::ErrorDetail::new(),
            diagnostic: String::from("test presenter error"),
        })
    }

    #[test]
    fn video_fit_preserves_aspect_ratio() {
        let geometry = TerminalGeometry::with_cell_size(80, 24, 10, 20);
        assert_eq!(display_size(1920, 1080, 1, 1, 1.0, geometry), (76, 22));
    }

    #[test]
    fn video_fit_applies_sample_aspect_ratio() {
        let geometry = TerminalGeometry::with_cell_size(120, 40, 10, 20);
        assert_eq!(display_size(320, 240, 2, 1, 1.0, geometry), (64, 12));
    }

    #[test]
    fn keyframe_recovery_restarts_at_the_recovery_position() {
        let mut state = PlaybackState::new();
        state.packet_id = 240;
        state.playback_phase = PlaybackPhase::Streaming;
        state.first_pts_us = Some(0);
        state.last_pts_us = Some(8_000_000);
        state.observe_audio_packet(8_000_000, 21_333);

        state.begin_recovery_restart();
        assert_eq!(state.playback_phase, PlaybackPhase::Pending);
        assert_eq!(
            state.first_pts_us, None,
            "the restarted PLAY must begin at the recovery keyframe, not the original stream start"
        );
        assert_eq!(state.audio_buffered_us, 0);
        assert_eq!(state.audio_horizon_us, None);
    }

    #[test]
    fn same_epoch_keyframe_recovery_preserves_playback_without_flush() {
        let mut state = PlaybackState::new();
        state.playback_phase = PlaybackPhase::Streaming;
        state.first_pts_us = Some(0);
        state.note_keyframe_request(KeyframeRequest {
            minimum_epoch: 1,
            reason: crate::protocol::messages::KEYFRAME_REASON_TRANSPORT_LOSS,
        });

        assert!(state.awaiting_keyframe);
        assert_eq!(state.epoch, 1);
        assert_eq!(state.take_recovery_action(), RecoveryAction::None);
        assert_eq!(state.playback_phase, PlaybackPhase::Streaming);
        assert_eq!(state.first_pts_us, Some(0));
    }

    #[test]
    fn same_epoch_initial_recovery_rebases_started_playback() {
        let mut state = PlaybackState::new();
        state.packet_id = 240;
        state.playback_phase = PlaybackPhase::Streaming;
        state.note_keyframe_request(KeyframeRequest {
            minimum_epoch: 1,
            reason: crate::protocol::messages::KEYFRAME_REASON_INITIAL,
        });

        assert_eq!(
            state.take_recovery_action(),
            RecoveryAction::RebasePlayback,
            "a recreated decoder needs a fresh PLAY at its recovery keyframe"
        );
        assert_eq!(state.take_recovery_action(), RecoveryAction::None);
    }

    #[test]
    fn visibility_resume_rebases_play_at_the_pending_packet() {
        let mut state = PlaybackState::new();
        state.first_pts_us = Some(0);
        state.last_pts_us = Some(8_000_000);

        assert_eq!(
            state.visibility_resume_pts(8_033_333),
            8_033_333,
            "PLAY must resume where packet submission resumes, not at the stream origin"
        );
        assert_eq!(
            state.visibility_resume_pts(i64::MIN),
            8_000_000,
            "a packet without a timestamp falls back to the last known media position"
        );
    }

    #[test]
    fn greater_epoch_keyframe_recovery_flushes_once_and_never_moves_backwards() {
        let mut state = PlaybackState::new();
        state.playback_phase = PlaybackPhase::Streaming;
        state.note_keyframe_request(KeyframeRequest {
            minimum_epoch: 4,
            reason: crate::protocol::messages::KEYFRAME_REASON_DECODER_ERROR,
        });
        state.note_keyframe_request(KeyframeRequest {
            minimum_epoch: 2,
            reason: crate::protocol::messages::KEYFRAME_REASON_TRANSPORT_LOSS,
        });

        assert_eq!(state.epoch, 4);
        assert_eq!(state.take_recovery_action(), RecoveryAction::Flush);
        assert_eq!(
            state.take_recovery_action(),
            RecoveryAction::None,
            "FLUSH is required exactly once"
        );
    }

    #[test]
    fn presenter_start_wait_is_joined_only_after_ingress_closes() {
        assert!(!PlaybackPhase::Pending.may_join_presenter_wait());
        assert!(
            !PlaybackPhase::Streaming.may_join_presenter_wait(),
            "blocking while H.264 packets are still streaming can starve decoder reordering"
        );
        assert!(PlaybackPhase::IngressClosed.may_join_presenter_wait());
    }

    #[test]
    fn long_playback_completion_waits_use_presenter_bounded_slices() {
        let mut timeouts = Vec::new();
        let mut attempts = 0;
        wait_in_source_timeout_slices(Duration::from_secs(95), |timeout| {
            timeouts.push(timeout);
            attempts += 1;
            if attempts < 4 {
                Err(presenter_error(crate::protocol::messages::ERROR_TIMEOUT))
            } else {
                Ok(())
            }
        })
        .unwrap();

        assert_eq!(
            timeouts,
            [
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(5),
            ]
        );
    }

    #[test]
    fn playback_completion_wait_does_not_hide_non_timeout_errors() {
        let error = wait_in_source_timeout_slices(Duration::from_secs(60), |_| {
            Err(presenter_error(
                crate::protocol::messages::ERROR_LIMIT_EXCEEDED,
            ))
        })
        .unwrap_err();

        assert!(!is_presenter_timeout(&error));
    }

    #[test]
    fn linked_restart_after_recovery_keeps_decodable_delta_frames() {
        let delta = |pts_us| EncodedPacket {
            data: vec![1],
            pts_us,
            dts_us: pts_us,
            key: false,
        };
        let mut state = PlaybackState::new();
        let mut pending = VecDeque::from([
            delta(0),
            EncodedPacket {
                data: vec![2],
                pts_us: 33_000,
                dts_us: 33_000,
                key: true,
            },
        ]);
        assert!(linked_start_ready(&state, &mut pending));
        assert_eq!(pending.len(), 1, "leading delta frames cannot be decoded");
        let mut no_key = VecDeque::from([delta(0), delta(33_000)]);
        assert!(!linked_start_ready(&state, &mut no_key));
        assert!(no_key.is_empty());

        state.packet_id = 241;
        let mut recovered = VecDeque::from([delta(8_000_000), delta(8_033_000)]);
        assert!(linked_start_ready(&state, &mut recovered));
        assert_eq!(
            recovered.len(),
            2,
            "delta frames after the submitted recovery keyframe stay queued"
        );
    }

    #[test]
    fn audio_progress_tracks_prebuffer_and_timeline_horizon() {
        let mut state = PlaybackState::new();
        for pts_us in [-21_333, 0, 21_333, 42_667, 64_000] {
            state.observe_audio_packet(pts_us, 21_333);
        }
        assert!(state.audio_buffered_us >= AUDIO_PREBUFFER_US);
        assert_eq!(state.audio_horizon_us, Some(85_333));
    }

    #[test]
    fn linked_audio_prebuffers_on_its_own_worker() {
        let progress = Arc::new(AudioStreamProgress::default());
        let worker_progress = progress.clone();
        let worker = thread::spawn(move || {
            for pts_us in [0, 20_000, 40_000, 60_000, 80_000] {
                worker_progress.observe(pts_us, 20_000);
            }
            worker_progress.finish(false);
        });

        let snapshot = progress.wait_for_prebuffer(AUDIO_PREBUFFER_US, Duration::from_secs(1));
        worker.join().unwrap();
        let finished = progress.snapshot();
        assert_eq!(snapshot.buffered_us, AUDIO_PREBUFFER_US);
        assert_eq!(snapshot.horizon_us, Some(AUDIO_PREBUFFER_US as i64));
        assert!(finished.finished);
        assert!(!finished.failed);
    }
}
