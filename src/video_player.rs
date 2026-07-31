use std::io;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use vivid_protocol::MAX_TRACK_WAIT_TIMEOUT_US;
use vivid_protocol::media::{AudioPacket, VideoPacket};
use vivid_protocol::messages::{ERROR_TIMEOUT, LaneClass};
use vivid_protocol::revision::ChannelGeneration;
use vivid_protocol::track::{KindConfiguration, TrackConfiguration, TrackMode};
use vivid_sdk::{
    ChannelEvent, CoordinateModel, MILESTONE_OUTPUT_READY, RequestMetadata, SlotBinding,
    SurfaceDefinition, SurfaceDescriptor, SurfaceRole, Track, TrackChannel, TrackWaitCondition,
    VideoConfiguration,
};

use crate::audio_player;
use crate::audio_streamer::audio_track;
use crate::cli::Config;
use crate::client::VividClient;
use crate::ffmpeg::{AudioDemuxer, EncodedMediaPacket, VideoDemuxer, VideoInfo};
use crate::terminal_geometry::{
    TerminalGeometry, cells_for_pixels, place_surface, reserve_rows, resize_placed_surface,
};

const FIT_MARGIN_COLS: u16 = 4;
const FIT_MARGIN_ROWS: u16 = 2;
const AUDIO_PREBUFFER_US: u64 = 100_000;
const AUDIO_START_TIMEOUT: Duration = Duration::from_secs(5);
const MAXIMUM_LATENCY_US: u64 = 2_000_000;
const PLAYBACK_START_TIMEOUT: Duration = Duration::from_secs(30);
const PLAYBACK_COMPLETION_GRACE: Duration = Duration::from_secs(30);
const SLOT_VIDEO: u64 = 1;
const SLOT_AUDIO: u64 = 2;

#[derive(Clone, Copy, Debug, Default)]
struct AudioProgressState {
    buffered_us: u64,
    finished: bool,
    failed: bool,
}

#[derive(Default)]
struct AudioProgress {
    state: Mutex<AudioProgressState>,
    changed: Condvar,
}

impl AudioProgress {
    fn observe(&self, duration_us: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.buffered_us = state.buffered_us.saturating_add(duration_us);
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

    fn wait_for_prebuffer(&self) -> AudioProgressState {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (state, _) = self
            .changed
            .wait_timeout_while(state, AUDIO_START_TIMEOUT, |state| {
                state.buffered_us < AUDIO_PREBUFFER_US && !state.finished
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *state
    }

    fn snapshot(&self) -> AudioProgressState {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

struct AudioOutcome {
    channel: Arc<TrackChannel>,
    packet_id: u64,
    error: Option<io::Error>,
}

struct PresenterAudio {
    track: Track,
    channel: Arc<TrackChannel>,
    progress: Arc<AudioProgress>,
    worker: thread::JoinHandle<AudioOutcome>,
}

#[derive(Debug, Clone, Copy)]
struct VideoRecovery {
    minimum_epoch: u32,
    advance_epoch: bool,
}

pub fn play(
    config: &Config,
    client: &mut VividClient,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let info = VideoDemuxer::inspect(path)?;
    if info.colorimetry_inferred {
        client.verbose(format_args!(
            "video {}: inferred colorimetry as primaries={}, transfer={}, matrix={}, range={}",
            path.display(),
            info.color_primaries,
            info.transfer,
            info.matrix,
            info.range
        ));
    }
    let geometry = TerminalGeometry::settled_presenter(client);
    let (mut columns, mut rows) = display_size(
        info.width,
        info.height,
        info.sar_num,
        info.sar_den,
        config.zoom,
        geometry,
    );
    let surface_id = client.allocate_id()?;
    let node_id = client.allocate_id()?;
    let context_id = client.info().root_context_id;
    let surface = client.create_surface(
        video_surface(context_id, surface_id, path, info.width, info.height),
        &RequestMetadata::default(),
    )?;
    let mut placed_node = place_surface(client, &surface, node_id, columns, rows)?;
    if !config.is_dry_run() {
        reserve_rows(rows)?;
    }

    let video_configuration = video_track(client, &surface, &info)?;
    let mut video_probe = video_configuration.clone();
    video_probe.track_id = 0;
    if !client.probe_track(&video_probe)?.supported {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "presenter rejected the video track configuration",
        )
        .into());
    }
    let video_track = client.create_track(video_configuration, &RequestMetadata::default())?;
    let mut video_channel = Arc::new(client.open_track_channel(&video_track)?);

    let remote = std::env::var_os("VIVID_REMOTE").is_some();
    let mut presenter_audio = create_presenter_audio(client, &surface, path, info.audio.as_ref())?;
    let mut local_audio: Option<audio_player::AudioPlayback> = if info.has_audio
        && presenter_audio.is_none()
        && !config.no_wait
        && !config.is_dry_run()
        && !remote
    {
        match audio_player::prepare_video(path, info.first_pts_us) {
            Ok(audio) => Some(audio),
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
    if info.has_audio && presenter_audio.is_none() && remote {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "remote video audio requires a supported Vivid 1.5 audio track",
        )
        .into());
    }

    let mut demuxer = VideoDemuxer::open(path)?;
    let mut packet_id = 0_u64;
    let mut epoch = 1_u32;
    let mut awaiting_keyframe = true;
    let mut started = false;
    let mut first_pts = None;
    let mut last_pts = None;
    let mut recovery_rebase_pending = false;
    while let Some(media) = demuxer.next_media_packet()? {
        let EncodedMediaPacket::Video(packet) = media else {
            continue;
        };
        if let Some(geometry) = take_target_geometry(client, video_track.id())? {
            let size = display_size(
                info.width,
                info.height,
                info.sar_num,
                info.sar_den,
                config.zoom,
                geometry,
            );
            if size != (columns, rows) {
                resize_placed_surface(client, &mut placed_node, size.0, size.1)?;
                (columns, rows) = size;
            }
        }
        if let Some(recovery) = take_video_recovery(&video_channel)? {
            awaiting_keyframe = true;
            recovery_rebase_pending = started;
            if recovery.advance_epoch {
                epoch = epoch
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("video epoch exhausted"))?
                    .max(recovery.minimum_epoch);
                client.flush(&video_track, epoch)?;
            } else {
                epoch = epoch.max(recovery.minimum_epoch);
            }
        }
        if packet.data.is_empty() || (awaiting_keyframe && !packet.key) {
            continue;
        }
        if awaiting_keyframe {
            awaiting_keyframe = false;
        }
        if recovery_rebase_pending {
            // A replacement decoder starts at this random-access unit, while linked audio keeps
            // its original timestamp domain. Publish the new authoritative clock before the
            // keyframe so a nested presenter cannot hold that frame forever behind PLAY(0).
            client.play(&video_track, packet.pts_us, 1, MAXIMUM_LATENCY_US)?;
            recovery_rebase_pending = false;
        }
        packet_id = packet_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("video packet ID space exhausted"))?;
        let first_pts_before_send = first_pts;
        first_pts.get_or_insert(packet.pts_us);
        let send_result = if started {
            video_channel.send_video(VideoPacket {
                epoch,
                packet_id,
                pts_us: packet.pts_us,
                dts_us: packet.dts_us,
                duration_us: 0,
                key: packet.key,
                data: &packet.data,
            })
        } else {
            let channel = video_channel.clone();
            let data = packet.data.clone();
            let pts_us = packet.pts_us;
            let dts_us = packet.dts_us;
            let key = packet.key;
            let sender = thread::spawn(move || {
                channel.send_video(VideoPacket {
                    epoch,
                    packet_id,
                    pts_us,
                    dts_us,
                    duration_us: 0,
                    key,
                    data: &data,
                })
            });
            let (result, started_while_sending) =
                finish_send_while_observing_start(sender, || {
                    let output_ready =
                        client.query_track(&video_track)?.milestones & MILESTONE_OUTPUT_READY != 0;
                    if output_ready {
                        start_video_playback(
                            config,
                            client,
                            &surface,
                            &video_track,
                            &mut presenter_audio,
                            &mut local_audio,
                            first_pts.unwrap_or(pts_us),
                        )?;
                    }
                    Ok(output_ready)
                })?;
            started = started_while_sending;
            result
        };
        if let Err(error) = send_result {
            if first_pts_before_send.is_none() && !started {
                first_pts = None;
            }
            client.verbose(format_args!(
                "video track {} channel failed: {error}; advancing generation",
                video_track.id()
            ));
            client.query_track(&video_track)?;
            client.advance_channel(&video_track, 1, &RequestMetadata::default())?;
            video_channel = Arc::new(client.open_track_channel(&video_track)?);
            epoch = epoch
                .checked_add(1)
                .ok_or_else(|| io::Error::other("video epoch exhausted"))?;
            awaiting_keyframe = true;
            recovery_rebase_pending = started;
            continue;
        }
        last_pts = Some(packet.pts_us);

        if started
            && presenter_audio
                .as_ref()
                .is_some_and(|audio| audio.progress.snapshot().failed)
        {
            let failed = presenter_audio.take().expect("failed audio track exists");
            let _ = failed.worker.join();
            if let Err(error) = clear_audio_slot(client, &surface) {
                client.verbose(format_args!("could not clear failed audio slot: {error}"));
            }
        }

        let output_ready =
            !started && client.query_track(&video_track)?.milestones & MILESTONE_OUTPUT_READY != 0;
        if output_ready {
            start_video_playback(
                config,
                client,
                &surface,
                &video_track,
                &mut presenter_audio,
                &mut local_audio,
                first_pts.unwrap_or(packet.pts_us),
            )?;
            started = true;
        }
    }
    if packet_id == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "video has no keyframes").into());
    }
    if !started {
        start_video_playback(
            config,
            client,
            &surface,
            &video_track,
            &mut presenter_audio,
            &mut local_audio,
            first_pts.unwrap_or(0),
        )?;
    }

    let mut audio_to_drain = None;
    let mut presenter_audio_failed = false;
    if let Some(audio) = presenter_audio.take() {
        match audio.worker.join() {
            Ok(outcome) if outcome.error.is_none() => {
                outcome.channel.eos()?;
                audio_to_drain = Some(audio.track);
                client.verbose(format_args!(
                    "audio track completed after {} packets",
                    outcome.packet_id
                ));
            }
            Ok(outcome) => {
                presenter_audio_failed = true;
                client.verbose(format_args!(
                    "audio track stopped after {} packets: {}",
                    outcome.packet_id,
                    outcome
                        .error
                        .map_or_else(|| "channel stopped".into(), |error| error.to_string())
                ));
            }
            Err(_) => {
                presenter_audio_failed = true;
                client.verbose(format_args!("audio track worker panicked"));
            }
        }
    }
    if presenter_audio_failed && let Err(error) = clear_audio_slot(client, &surface) {
        client.verbose(format_args!("could not clear failed audio slot: {error}"));
    }
    video_channel.eos()?;
    if let Some(audio_track) = audio_to_drain.as_ref()
        && let Err(error) = client.drain(audio_track)
    {
        client.verbose(format_args!("presenter audio drain failed: {error}"));
    }
    if !config.no_wait {
        let timeline = last_pts
            .zip(first_pts)
            .map_or(0, |(last, first)| last.saturating_sub(first).max(0) as u64);
        let timeout = Duration::from_micros(timeline).saturating_add(PLAYBACK_COMPLETION_GRACE);
        wait_for_playback_end(client, &video_track, timeout)?;
        if let Some(audio) = local_audio.as_mut()
            && let Err(error) = audio.wait()
        {
            client.verbose(format_args!(
                "local audio completion failed for {}: {error}",
                path.display()
            ));
        }
    }
    client.verbose(format_args!(
        "video surface {surface_id}, track {}: {packet_id} packets presented at {columns}x{rows} cells",
        video_track.id()
    ));
    Ok(())
}

fn wait_for_playback_end(
    client: &VividClient,
    track: &Track,
    overall_timeout: Duration,
) -> io::Result<()> {
    let deadline = Instant::now()
        .checked_add(overall_timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "playback wait is too long"))?;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for Vivid playback to finish",
            ));
        }
        let request_timeout = remaining.min(Duration::from_micros(MAX_TRACK_WAIT_TIMEOUT_US));
        let request_timeout_us = timeout_us(request_timeout).max(1);
        match client.wait_track(
            track,
            TrackWaitCondition::PlaybackEnded,
            None,
            request_timeout_us,
        ) {
            Ok(_) => return Ok(()),
            Err(error)
                if presenter_code(&error) == Some(ERROR_TIMEOUT) && Instant::now() < deadline => {}
            Err(error) => return Err(error),
        }
    }
}

fn presenter_code(error: &io::Error) -> Option<u64> {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<vivid_sdk::PresenterError>())
        .map(|error| error.code)
}

fn clear_audio_slot(client: &mut VividClient, surface: &vivid_sdk::Surface) -> io::Result<()> {
    client
        .activate_tracks(
            surface,
            &[SlotBinding {
                slot: SLOT_AUDIO,
                track_id: 0,
                expected_channel_generation: ChannelGeneration::ZERO,
                required_milestone: 0,
            }],
            &RequestMetadata::default(),
        )
        .map(|_| ())
}

fn create_presenter_audio(
    client: &mut VividClient,
    surface: &vivid_sdk::Surface,
    path: &Path,
    info: Option<&crate::ffmpeg::AudioInfo>,
) -> io::Result<Option<PresenterAudio>> {
    let Some(info) = info else {
        return Ok(None);
    };
    let configuration = audio_track(client, surface, info)?;
    let mut probe = configuration.clone();
    probe.track_id = 0;
    if !client.probe_track(&probe)?.supported {
        return Ok(None);
    }
    let track = match client.create_track(configuration, &RequestMetadata::default()) {
        Ok(track) => track,
        Err(error) => {
            client.verbose(format_args!("presenter audio track unavailable: {error}"));
            return Ok(None);
        }
    };
    let channel = match client.open_track_channel(&track) {
        Ok(channel) => Arc::new(channel),
        Err(error) => {
            client.verbose(format_args!("presenter audio channel unavailable: {error}"));
            return Ok(None);
        }
    };
    let progress = Arc::new(AudioProgress::default());
    let worker_progress = progress.clone();
    let worker_channel = channel.clone();
    let path = path.to_path_buf();
    let worker = thread::spawn(move || {
        let result = stream_audio(&path, &worker_channel, &worker_progress);
        worker_progress.finish(result.is_err());
        match result {
            Ok(packet_id) => AudioOutcome {
                channel: worker_channel,
                packet_id,
                error: None,
            },
            Err(error) => AudioOutcome {
                channel: worker_channel,
                packet_id: 0,
                error: Some(error),
            },
        }
    });
    Ok(Some(PresenterAudio {
        track,
        channel,
        progress,
        worker,
    }))
}

fn stream_audio(path: &Path, channel: &TrackChannel, progress: &AudioProgress) -> io::Result<u64> {
    let mut demuxer = AudioDemuxer::open(path)?;
    let mut packet_id = 0_u64;
    while let Some(packet) = demuxer.next_packet()? {
        if packet.data.is_empty() {
            continue;
        }
        packet_id = packet_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("audio packet ID space exhausted"))?;
        channel.send_audio(AudioPacket {
            epoch: 1,
            packet_id,
            pts_us: packet.pts_us,
            dts_us: packet.dts_us,
            duration_us: packet.duration_us,
            trim_start_samples: packet.trim_start_samples,
            trim_end_samples: packet.trim_end_samples,
            data: &packet.data,
        })?;
        progress.observe(packet.duration_us);
    }
    Ok(packet_id)
}

#[allow(clippy::too_many_arguments)]
fn activate_and_play(
    config: &Config,
    client: &mut VividClient,
    surface: &vivid_sdk::Surface,
    video: &Track,
    audio: Option<&Track>,
    start_pts_us: i64,
    minimum_buffer_us: u64,
) -> io::Result<bool> {
    client.wait_track(
        video,
        TrackWaitCondition::MilestoneSet,
        Some(MILESTONE_OUTPUT_READY),
        timeout_us(PLAYBACK_START_TIMEOUT),
    )?;
    let mut bindings = vec![SlotBinding {
        slot: SLOT_VIDEO,
        track_id: video.id(),
        expected_channel_generation: video.channel_generation(),
        required_milestone: MILESTONE_OUTPUT_READY,
    }];
    let mut audio_active = audio.is_some();
    if let Some(audio) = audio
        && let Err(wait_error) = client.wait_track(
            audio,
            TrackWaitCondition::MilestoneSet,
            Some(MILESTONE_OUTPUT_READY),
            timeout_us(AUDIO_START_TIMEOUT),
        )
    {
        let status = client.query_track(audio)?;
        if status.lifecycle == 6 || presenter_code(&wait_error) == Some(ERROR_TIMEOUT) {
            client.verbose(format_args!(
                "presenter audio track {} was not ready before activation ({wait_error}); continuing with video",
                audio.id()
            ));
            audio_active = false;
        } else {
            return Err(wait_error);
        }
    }
    if audio_active {
        let audio = audio.expect("active presenter audio track exists");
        bindings.push(SlotBinding {
            slot: SLOT_AUDIO,
            track_id: audio.id(),
            expected_channel_generation: audio.channel_generation(),
            required_milestone: MILESTONE_OUTPUT_READY,
        });
    }
    if let Err(activation_error) =
        client.activate_tracks(surface, &bindings, &RequestMetadata::default())
    {
        let video_status = client.query_track(video)?;
        let audio_lost = if let Some(audio) = audio {
            client
                .query_track(audio)
                .is_ok_and(|status| status.lifecycle == 6)
        } else {
            false
        };
        if audio_active
            && audio_lost
            && video_status.lifecycle != 6
            && video_status.channel_generation == video.channel_generation()
            && video_status.milestones & MILESTONE_OUTPUT_READY != 0
        {
            client.verbose(format_args!(
                "presenter audio was lost during atomic activation ({activation_error}); retrying video alone"
            ));
            audio_active = false;
            bindings.truncate(1);
            client.activate_tracks(surface, &bindings, &RequestMetadata::default())?;
        } else {
            return Err(activation_error);
        }
    }
    let clock = if audio_active {
        audio.expect("active presenter audio track exists")
    } else {
        video
    };
    client.play(clock, start_pts_us, minimum_buffer_us, MAXIMUM_LATENCY_US)?;
    if !config.no_wait {
        client.wait_track(
            clock,
            TrackWaitCondition::PlaybackStarted,
            None,
            timeout_us(PLAYBACK_START_TIMEOUT),
        )?;
    }
    Ok(audio_active)
}

fn start_video_playback(
    config: &Config,
    client: &mut VividClient,
    surface: &vivid_sdk::Surface,
    video: &Track,
    presenter_audio: &mut Option<PresenterAudio>,
    local_audio: &mut Option<audio_player::AudioPlayback>,
    start_pts_us: i64,
) -> io::Result<()> {
    let audio_ready = presenter_audio
        .as_ref()
        .map(|audio| audio.progress.wait_for_prebuffer())
        .filter(|state| {
            !state.failed && (state.buffered_us >= AUDIO_PREBUFFER_US || state.finished)
        });
    if presenter_audio.is_some() && audio_ready.is_none() {
        client.verbose(format_args!(
            "presenter audio did not prebuffer before activation; continuing with video"
        ));
        cancel_presenter_audio(client, presenter_audio);
    }
    let audio_active = activate_and_play(
        config,
        client,
        surface,
        video,
        presenter_audio.as_ref().map(|audio| &audio.track),
        start_pts_us,
        audio_ready.map_or(0, |state| state.buffered_us.clamp(1, AUDIO_PREBUFFER_US)),
    )?;
    if !audio_active {
        cancel_presenter_audio(client, presenter_audio);
    }
    if let Some(audio) = local_audio.as_ref()
        && let Err(error) = audio.start()
    {
        client.verbose(format_args!(
            "local audio disabled after output start failed: {error}"
        ));
        *local_audio = None;
    }
    Ok(())
}

fn cancel_presenter_audio(client: &mut VividClient, presenter_audio: &mut Option<PresenterAudio>) {
    let Some(audio) = presenter_audio.take() else {
        return;
    };
    let _ = audio.channel.close();
    if let Err(error) = client.destroy_track(&audio.track, &RequestMetadata::default()) {
        client.verbose(format_args!(
            "could not destroy unusable presenter audio track {}: {error}",
            audio.track.id()
        ));
    }
    // Dropping a JoinHandle detaches the worker. The channel and track cancellation above wake
    // ordinary flow waits; detaching also guarantees a transport write that is already inside the
    // operating system cannot hold video startup hostage.
    drop(audio.worker);
}

fn finish_send_while_observing_start<T, F>(
    sender: thread::JoinHandle<io::Result<T>>,
    mut observe_start: F,
) -> io::Result<(io::Result<T>, bool)>
where
    T: Send + 'static,
    F: FnMut() -> io::Result<bool>,
{
    let mut started = false;
    let deadline = Instant::now()
        .checked_add(PLAYBACK_START_TIMEOUT)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "startup wait is too long"))?;
    while !sender.is_finished() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out while a priming video write was blocked",
            ));
        }
        if !started && observe_start()? {
            started = true;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let result = sender
        .join()
        .map_err(|_| io::Error::other("video sender thread panicked"))?;
    Ok((result, started))
}

fn take_video_recovery(channel: &TrackChannel) -> io::Result<Option<VideoRecovery>> {
    let mut recovery = None;
    while let Some(event) = channel.take_event()? {
        match event {
            ChannelEvent::NeedKeyframe(payload) => {
                let minimum_epoch = payload
                    .iter()
                    .find(|(key, _)| *key == 4)
                    .and_then(|(_, value)| value.as_u64())
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or(1);
                let reason = payload
                    .iter()
                    .find(|(key, _)| *key == 5)
                    .and_then(|(_, value)| value.as_u64())
                    .unwrap_or(2);
                recovery = Some(VideoRecovery {
                    minimum_epoch,
                    advance_epoch: reason != 5,
                });
            }
            ChannelEvent::Error(error) => return Err(io::Error::other(error)),
            ChannelEvent::NeedFullFrame(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "video channel received a raster recovery request",
                ));
            }
        }
    }
    Ok(recovery)
}

fn take_target_geometry(
    client: &mut VividClient,
    video_track_id: u64,
) -> io::Result<Option<TerminalGeometry>> {
    let mut changed = false;
    while let Some(event) = client.take_event()? {
        match event {
            vivid_sdk::SessionEvent::TargetChanged(payload) => {
                client.apply_target_changed(&payload)?;
                changed = true;
            }
            vivid_sdk::SessionEvent::TrackLost { object_id, .. } if object_id == video_track_id => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "presenter reported the video track lost",
                ));
            }
            vivid_sdk::SessionEvent::ConnectionClosed { diagnostic } => {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, diagnostic));
            }
            _ => {}
        }
    }
    if !changed {
        return Ok(None);
    }
    match TerminalGeometry::from_target_descriptor(&client.info().target_descriptor) {
        Ok(geometry) => Ok(Some(geometry)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

fn video_surface(
    context_id: u64,
    surface_id: u64,
    path: &Path,
    width: u32,
    height: u32,
) -> SurfaceDefinition {
    SurfaceDefinition {
        context_id,
        surface_id,
        semantic_profile: vivid_sdk::GENERIC_CONTENT.into(),
        coordinate_model: CoordinateModel::DesktopLogicalPixels,
        logical_width: u64::from(width),
        logical_height: u64::from(height),
        scale_numerator: 1,
        scale_denominator: 1,
        rotation: 0,
        descriptor: SurfaceDescriptor {
            role: SurfaceRole::TimedMedia,
            title: path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("video")
                .chars()
                .take(256)
                .collect(),
            semantic_content_revision: 1,
            semantic_availability: 0,
            locator_hint: String::new(),
        },
        policy: 0,
        profile_parameters: vec![],
    }
}

fn video_track(
    client: &VividClient,
    surface: &vivid_sdk::Surface,
    info: &VideoInfo,
) -> io::Result<TrackConfiguration> {
    let maximum_record_body = vivid_protocol::media::video_body_len(info.max_access_unit_bytes)
        .map_err(io::Error::other)?;
    let retained_pixel_charge = u64::from(info.width)
        .checked_mul(u64::from(info.height))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "video pixels overflow"))?;
    Ok(TrackConfiguration {
        context_id: surface.context_id(),
        surface_id: surface.id(),
        track_id: client.allocate_id()?,
        slot: SLOT_VIDEO,
        mode: TrackMode::Timed,
        lane: LaneClass::Bulk,
        maximum_record_body,
        maximum_rate_millihertz: info.maximum_rate_millihertz.max(1),
        maximum_encoded_bits_per_second: info.maximum_encoded_bits_per_second.max(1),
        maximum_records_per_second: info.maximum_records_per_second.max(1),
        maximum_inflight_body_bytes: u64::from(maximum_record_body).saturating_mul(8),
        kind: KindConfiguration::Video(VideoConfiguration {
            codec: info.codec.clone(),
            packetization: info.packetization.clone(),
            extradata: info.extradata.clone(),
            coded_width: info.width,
            coded_height: info.height,
            profile: info.profile,
            level: info.level,
            maximum_reorder_depth: 16,
            color_primaries: info.color_primaries,
            transfer: info.transfer,
            matrix: info.matrix,
            signal_range: info.range,
            aspect_numerator: u64::from(info.sar_num.max(1)),
            aspect_denominator: u64::from(info.sar_den.max(1)),
            maximum_access_unit_bytes: info.max_access_unit_bytes,
            codec_string: info.codec_string.clone(),
            decoder_configuration: info.decoder_config.clone(),
        }),
        target_latency_us: 0,
        maximum_latency_us: MAXIMUM_LATENCY_US,
        retained_pixel_charge,
    })
}

fn display_size(
    width: u32,
    height: u32,
    sar_num: u32,
    sar_den: u32,
    zoom: f32,
    geometry: TerminalGeometry,
) -> (u32, u32) {
    let sar = f64::from(sar_num.max(1)) / f64::from(sar_den.max(1));
    let desired_width = f64::from(width) * sar * f64::from(zoom);
    let desired_height = f64::from(height) * f64::from(zoom);
    let max_width = f64::from(geometry.drawable_width_px(FIT_MARGIN_COLS));
    let max_height = f64::from(geometry.drawable_height_px(FIT_MARGIN_ROWS));
    let scale = (max_width / desired_width)
        .min(max_height / desired_height)
        .min(1.0);
    let pixel_width = (desired_width * scale).round().clamp(1.0, max_width) as u32;
    let pixel_height = (desired_height * scale).round().clamp(1.0, max_height) as u32;
    (
        cells_for_pixels(pixel_width, geometry.cell_width_px),
        cells_for_pixels(pixel_height, geometry.cell_height_px),
    )
}

fn timeout_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_size_accounts_for_sample_aspect_ratio() {
        let geometry = TerminalGeometry::with_cell_size(100, 40, 10, 20);
        assert_eq!(display_size(640, 360, 1, 1, 1.0, geometry), (64, 18));
        assert_eq!(display_size(320, 360, 2, 1, 1.0, geometry), (64, 18));
    }

    #[test]
    fn audio_prebuffer_wait_finishes_on_failure() {
        let progress = AudioProgress::default();
        progress.finish(true);
        assert!(progress.wait_for_prebuffer().failed);
    }

    #[test]
    fn blocked_priming_send_can_be_released_by_control_plane_start() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let sender_gate = gate.clone();
        let sender = thread::spawn(move || {
            let (lock, changed) = &*sender_gate;
            let started = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let _started = changed
                .wait_while(started, |started| !*started)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Ok(7_u64)
        });
        let observer_gate = gate.clone();
        let (result, started) = finish_send_while_observing_start(sender, move || {
            let (lock, changed) = &*observer_gate;
            *lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
            changed.notify_all();
            Ok(true)
        })
        .unwrap();
        assert!(started);
        assert_eq!(result.unwrap(), 7);
    }

    #[test]
    fn playback_completion_wait_requests_are_protocol_bounded() {
        let long_wait = PLAYBACK_COMPLETION_GRACE.saturating_add(Duration::from_secs(90));
        assert!(timeout_us(long_wait) > MAX_TRACK_WAIT_TIMEOUT_US);
        assert_eq!(
            timeout_us(long_wait.min(Duration::from_micros(MAX_TRACK_WAIT_TIMEOUT_US))),
            MAX_TRACK_WAIT_TIMEOUT_US
        );
        assert_eq!(timeout_us(Duration::from_nanos(1)).max(1), 1);
    }
}
