use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::time::Duration;

use crate::audio_player;
use crate::cli::Config;
use crate::client::{
    KeyframeRequest, MediaSender, PresenterError, SourceWaitHandle, VividClient, WaitSource,
};
use crate::ffmpeg::{EncodedMediaPacket, EncodedPacket, VideoDemuxer};
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
    audio_buffered_us: u64,
    audio_horizon_us: Option<i64>,
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
            audio_buffered_us: 0,
            audio_horizon_us: None,
        }
    }

    fn note_keyframe_request(&mut self, request: KeyframeRequest) {
        if request.minimum_epoch > self.epoch {
            self.epoch = request.minimum_epoch;
            self.recovery_requires_flush = true;
        }
        self.awaiting_keyframe = true;
    }

    fn take_recovery_flush(&mut self) -> bool {
        self.awaiting_keyframe = false;
        std::mem::take(&mut self.recovery_requires_flush)
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

fn audio_covers_video(packet: &EncodedPacket, audio_horizon_us: Option<i64>) -> bool {
    let timestamp = if packet.pts_us != i64::MIN {
        packet.pts_us
    } else {
        packet.dts_us
    };
    timestamp == i64::MIN || audio_horizon_us.is_some_and(|horizon| timestamp <= horizon)
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
                eprintln!(
                    "vivi: warning: could not start audio for {}: {error}; continuing without sound",
                    path.display()
                );
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
            if state.awaiting_keyframe && state.take_recovery_flush() {
                self.client.flush(self.source_id, state.epoch)?;
                state.begin_recovery_restart();
            }
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
                    state.first_pts_us.unwrap_or(0),
                    INITIAL_BUFFER_US,
                )?;
                if state.audio_started
                    && let Some(playback) = self.audio.as_ref()
                {
                    playback.resume();
                }
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
        eprintln!(
            "vivi: warning: {} does not declare complete colorimetry; using primaries={}, transfer={}, matrix={}, range={}",
            path.display(),
            info.color_primaries,
            info.transfer,
            info.matrix,
            info.range
        );
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
                eprintln!(
                    "vivi: warning: could not prepare audio for {}: {error}; continuing without sound",
                    path.display()
                );
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
    let mut vivid_audio = if let Some((audio_id, presenter_audio)) = presenter_audio {
        match presenter_audio {
            Ok(audio_source) => {
                let audio_sender = client.open_media_sender(audio_source, ConnectionKind::Audio)?;
                Some((audio_id, audio_sender, 0_u64))
            }
            Err(error) => {
                if !remote_session && !config.is_dry_run() {
                    match audio_player::prepare_video(path, info.first_pts_us) {
                        Ok(playback) => audio = Some(playback),
                        Err(fallback_error) => eprintln!(
                            "vivi: warning: could not create presenter audio for {}: {error}; direct audio fallback also failed: {fallback_error}; continuing without sound",
                            path.display(),
                        ),
                    }
                } else {
                    eprintln!(
                        "vivi: warning: could not create presenter audio for {}: {error}; continuing without sound",
                        path.display()
                    );
                }
                None
            }
        }
    } else {
        if info.has_audio && remote_session && !config.no_wait {
            eprintln!(
                "vivi: warning: presenter lacks remote audio for {}; continuing without sound",
                path.display()
            );
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

    let mut state = PlaybackState::new();
    // The media channels are independent, so preserve each stream's decode order rather than the
    // container's cross-stream packet order. Some MP4s front-load enough video to fill the video
    // socket before their first audio packet; buffering those video access units lets audio reach
    // the presenter and start its master clock without a circular wait.
    let mut pending_video = VecDeque::new();
    while let Some(media_packet) = demuxer.next_media_packet()? {
        match media_packet {
            EncodedMediaPacket::Audio(packet) => {
                let mut failed = None;
                if let Some((_, audio_sender, packet_id)) = vivid_audio.as_mut()
                    && !packet.data.is_empty()
                {
                    *packet_id = packet_id
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("audio packet ID space exhausted"))?;
                    if let Err(error) = audio_sender.send_audio(AudioPacket {
                        epoch: state.epoch,
                        packet_id: *packet_id,
                        pts_us: packet.pts_us,
                        dts_us: packet.dts_us,
                        duration_us: packet.duration_us,
                        trim_start_samples: packet.trim_start_samples,
                        trim_end_samples: packet.trim_end_samples,
                        data: &packet.data,
                    }) {
                        failed = Some(error);
                    } else {
                        state.observe_audio_packet(packet.pts_us, packet.duration_us);
                    }
                }
                if let Some(error) = failed {
                    eprintln!(
                        "vivi: warning: presenter audio failed for {}: {error}; continuing without sound",
                        path.display()
                    );
                    vivid_audio = None;
                }
            }
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

        if vivid_audio.is_some() {
            if !state.playback_phase.admitted()
                && linked_start_ready(&state, &mut pending_video)
                && state.audio_buffered_us >= AUDIO_PREBUFFER_US
            {
                start_playback(
                    client,
                    source_id,
                    AUDIO_PREBUFFER_US,
                    &mut audio,
                    &mut state,
                    path,
                )?;
            }
            while state.playback_phase.admitted()
                && pending_video
                    .front()
                    .is_some_and(|packet| audio_covers_video(packet, state.audio_horizon_us))
            {
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
    client.eos_sender(&sender, state.epoch)?;
    state.playback_phase = PlaybackPhase::IngressClosed;
    debug_assert!(state.playback_phase.may_join_presenter_wait());
    if let Some(mut wait) = state.playback_wait.take() {
        wait.wait()?;
    }
    if let Some((audio_id, audio_sender, _)) = vivid_audio.as_ref()
        && let Err(error) = client
            .eos_sender(audio_sender, state.epoch)
            .and_then(|_| client.drain(*audio_id))
    {
        eprintln!(
            "vivi: warning: presenter audio drain failed for {}: {error}; video playback completed",
            path.display()
        );
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
        eprintln!(
            "vivi: warning: audio playback failed for {}: {error}; video playback completed",
            path.display()
        );
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
        assert!(!state.take_recovery_flush());
        assert_eq!(state.playback_phase, PlaybackPhase::Streaming);
        assert_eq!(state.first_pts_us, Some(0));
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
        assert!(state.take_recovery_flush());
        assert!(
            !state.take_recovery_flush(),
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
    fn audio_horizon_gates_video_from_front_loaded_muxes() {
        let mut state = PlaybackState::new();
        for pts_us in [-21_333, 0, 21_333, 42_667, 64_000] {
            state.observe_audio_packet(pts_us, 21_333);
        }
        assert!(state.audio_buffered_us >= AUDIO_PREBUFFER_US);
        assert_eq!(state.audio_horizon_us, Some(85_333));

        let covered = EncodedPacket {
            data: vec![1],
            pts_us: 83_333,
            dts_us: 0,
            key: true,
        };
        let video_ahead_of_audio = EncodedPacket {
            data: vec![2],
            pts_us: 250_000,
            dts_us: 41_667,
            key: false,
        };
        assert!(audio_covers_video(&covered, state.audio_horizon_us));
        assert!(!audio_covers_video(
            &video_ahead_of_audio,
            state.audio_horizon_us
        ));
        assert!(!audio_covers_video(&covered, None));
    }
}
