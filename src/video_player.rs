use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
use crate::client::{VividClient, catchup_delivery_rates};
use crate::ffmpeg::{AudioDemuxer, EncodedMediaPacket, VideoDemuxer, VideoInfo};
use crate::playback_ui::{Command, PlaybackUi};
use crate::terminal_geometry::{
    TerminalGeometry, cells_for_pixels, place_full_window_surface, place_surface, reserve_rows,
    resize_placed_surface, update_full_window_surface,
};

const FIT_MARGIN_COLS: u16 = 4;
const FIT_MARGIN_ROWS: u16 = 2;
const AUDIO_PREBUFFER_US: u64 = 100_000;
const AUDIO_START_TIMEOUT: Duration = Duration::from_secs(5);
const MAXIMUM_LATENCY_US: u64 = 2_000_000;
const PLAYBACK_START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a seek's pre-roll may still be in flight before its silence means end of file.
const SEEK_OUTPUT_GRACE: Duration = Duration::from_secs(2);
const PLAYBACK_COMPLETION_GRACE: Duration = Duration::from_secs(30);
const SLOT_VIDEO: u64 = 1;
const SLOT_AUDIO: u64 = 2;

#[derive(Clone, Copy, Debug, Default)]
struct AudioProgressState {
    buffered_us: u64,
    last_submitted_end_pts_us: Option<i64>,
    finished: bool,
    failed: bool,
}

#[derive(Default)]
struct AudioProgress {
    state: Mutex<AudioProgressState>,
    changed: Condvar,
}

impl AudioProgress {
    fn observe(&self, pts_us: i64, duration_us: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.buffered_us = state.buffered_us.saturating_add(duration_us);
        if pts_us != i64::MIN {
            let end_pts_us = pts_us.saturating_add(i64::try_from(duration_us).unwrap_or(i64::MAX));
            state.last_submitted_end_pts_us = Some(
                state
                    .last_submitted_end_pts_us
                    .map_or(end_pts_us, |current| current.max(end_pts_us)),
            );
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

/// Latest media PTS linked audio must reach before resuming after video recovery.
///
/// The video and audio demuxers intentionally run on independent threads. A replacement decoder
/// can skip undecodable video interframes in memory, but without this handoff the audio worker
/// still submits every sample covering that skipped interval. Its normal rate limiter then turns
/// a multi-second GOP into a multi-second blank surface while the outer presenter waits to
/// activate both tracks. Coalescing to the greatest requested PTS keeps repeated recovery
/// requests monotonic and lets the audio demuxer discard that interval locally instead.
#[derive(Default)]
struct AudioRecoveryTarget {
    pts_us: Mutex<Option<i64>>,
}

impl AudioRecoveryTarget {
    fn request(&self, pts_us: i64) {
        if pts_us == i64::MIN {
            return;
        }
        let mut target = self
            .pts_us
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *target = Some(target.map_or(pts_us, |current| current.max(pts_us)));
    }

    fn take(&self) -> Option<i64> {
        self.pts_us
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

fn merge_recovery_target(target: &mut Option<i64>, requested: Option<i64>) {
    if let Some(requested) = requested {
        *target = Some(target.map_or(requested, |current| current.max(requested)));
    }
}

fn audio_packet_reaches_recovery_target(target: &mut Option<i64>, pts_us: i64) -> bool {
    match *target {
        Some(target_pts) if pts_us == i64::MIN || pts_us < target_pts => false,
        Some(_) => {
            *target = None;
            true
        }
        None => true,
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
    packet_ids: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    progress: Arc<AudioProgress>,
    worker: thread::JoinHandle<AudioOutcome>,
}

struct DrainedPresenterAudio {
    track: Track,
    packet_ids: Arc<AtomicU64>,
}

struct AudioStreamState<'a> {
    packet_ids: &'a AtomicU64,
    stop: &'a AtomicBool,
    progress: &'a AudioProgress,
    recovery: &'a AudioRecoveryTarget,
}

#[derive(Debug, Clone, Copy)]
struct VideoRecovery {
    minimum_epoch: u32,
    advance_epoch: bool,
}

enum AudioWaitEvent {
    Recovery(VideoRecovery),
    Quit,
    Seek(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryControl {
    Pause,
    Advance,
    Flush,
    Open,
}

fn recovery_control_order(started: bool) -> impl Iterator<Item = RecoveryControl> {
    [
        started.then_some(RecoveryControl::Pause),
        Some(RecoveryControl::Advance),
        Some(RecoveryControl::Flush),
        Some(RecoveryControl::Open),
    ]
    .into_iter()
    .flatten()
}

pub fn play(
    config: &Config,
    client: &mut VividClient,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let info = VideoDemuxer::inspect(path)?;
    let ui = PlaybackUi::enter(
        config,
        path,
        info.duration_us,
        info.has_audio && client.supports(vivid_protocol::registry::AUDIO_GAIN),
        false,
    )?;
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
    let layout_geometry = media_geometry(geometry, ui.is_some());
    let (mut columns, mut rows) = display_size(
        info.width,
        info.height,
        info.sar_num,
        info.sar_den,
        config.zoom,
        layout_geometry,
    );
    let surface_id = client.allocate_id()?;
    let node_id = client.allocate_id()?;
    let context_id = client.info().root_context_id;
    let surface = client.create_surface(
        video_surface(context_id, surface_id, path, info.width, info.height),
        &RequestMetadata::default(),
    )?;
    let (mut column, mut row) = centered_origin(layout_geometry, columns, rows);
    let mut placed_node = if ui.is_some() {
        place_full_window_surface(client, &surface, node_id, column, row, columns, rows)?
    } else {
        place_surface(client, &surface, node_id, columns, rows)?
    };
    if !config.is_dry_run() && ui.is_none() {
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
    let audio_recovery = Arc::new(AudioRecoveryTarget::default());
    let mut volume_percent = 100_u32;
    let mut presenter_audio = create_presenter_audio(
        client,
        &surface,
        path,
        info.audio.as_ref(),
        audio_recovery.clone(),
        None,
    )?;
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
    let mut recovery_start_pts_us = None;
    let timeline_origin_us = info.first_pts_us.unwrap_or(0);
    let mut playback_base_us = 0_u64;
    let mut playback_started_at: Option<Instant> = None;
    let mut play_start_override = None;
    let mut readiness = ReadinessPoll::default();
    'playback: loop {
        let packets_before_generation = packet_id;
        while let Some(media) = demuxer.next_media_packet()? {
            let EncodedMediaPacket::Video(packet) = media else {
                continue;
            };
            if let Some(geometry) = take_target_geometry(client, video_track.id())? {
                let layout_geometry = media_geometry(geometry, ui.is_some());
                let size = display_size(
                    info.width,
                    info.height,
                    info.sar_num,
                    info.sar_den,
                    config.zoom,
                    layout_geometry,
                );
                if size != (columns, rows) {
                    if ui.is_some() {
                        (column, row) = centered_origin(layout_geometry, size.0, size.1);
                        update_full_window_surface(
                            client,
                            &mut placed_node,
                            column,
                            row,
                            size.0,
                            size.1,
                        )?;
                    } else {
                        resize_placed_surface(client, &mut placed_node, size.0, size.1)?;
                    }
                    (columns, rows) = size;
                }
            }
            if let Some(ui) = ui.as_ref() {
                let current_us = playback_base_us.saturating_add(
                    playback_started_at
                        .map(|started| {
                            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
                        })
                        .unwrap_or(0),
                );
                ui.set_position_us(current_us.min(info.duration_us.unwrap_or(u64::MAX)));
                let mut seek_target = None;
                while let Some(command) = ui.try_command() {
                    match command {
                        Command::Quit => {
                            let _ = video_channel.close();
                            cancel_presenter_audio(client, &mut presenter_audio);
                            cleanup_full_window(
                                client,
                                &surface,
                                &video_track,
                                placed_node.node_id,
                            );
                            return Ok(());
                        }
                        Command::Resize(geometry) => {
                            let layout_geometry = media_geometry(geometry, true);
                            let size = display_size(
                                info.width,
                                info.height,
                                info.sar_num,
                                info.sar_den,
                                config.zoom,
                                layout_geometry,
                            );
                            (column, row) = centered_origin(layout_geometry, size.0, size.1);
                            update_full_window_surface(
                                client,
                                &mut placed_node,
                                column,
                                row,
                                size.0,
                                size.1,
                            )?;
                            (columns, rows) = size;
                            ui.redraw()?;
                        }
                        Command::VolumeBy(delta) => {
                            let next = (volume_percent as i32 + delta).clamp(0, 200) as u32;
                            if let Some(audio) = presenter_audio.as_ref()
                                && client.supports(vivid_protocol::registry::AUDIO_GAIN)
                            {
                                client.set_audio_gain(
                                    &audio.track,
                                    vivid_sdk::AudioGain::from_percent(next)
                                        .expect("clamped volume is valid"),
                                )?;
                                volume_percent = next;
                                ui.set_volume_percent(Some(next));
                                ui.set_message(format!("Volume {next}%"));
                            } else if let Some(audio) = local_audio.as_ref() {
                                audio.set_volume_percent(next);
                                volume_percent = next;
                                ui.set_volume_percent(Some(next));
                                ui.set_message(format!("Volume {next}%"));
                            } else {
                                ui.set_volume_percent(None);
                                ui.set_message("Volume unavailable");
                            }
                            ui.redraw()?;
                        }
                        Command::SeekBy(delta) => {
                            seek_target = Some(if delta < 0 {
                                current_us.saturating_sub(delta.unsigned_abs())
                            } else {
                                current_us.saturating_add(delta as u64)
                            });
                        }
                        Command::SeekTo(target) => seek_target = Some(target),
                    }
                }
                if let Some(target) = seek_target {
                    let target = target.min(info.duration_us.unwrap_or(u64::MAX));
                    let target_pts = timeline_origin_us
                        .saturating_add(i64::try_from(target).unwrap_or(i64::MAX));
                    if started {
                        client.pause(&video_track)?;
                    }
                    epoch = epoch
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("video epoch exhausted"))?;
                    replace_video_channel(
                        client,
                        &video_track,
                        &mut video_channel,
                        epoch,
                        false,
                        1,
                    )?;
                    demuxer.seek_to_us(target_pts)?;
                    if presenter_audio.is_some() {
                        // Retire the slot before advancing the retained audio track generation.
                        clear_audio_slot(client, &surface)?;
                    }
                    let _ = audio_recovery.take();
                    if let Some(audio) = presenter_audio.take() {
                        presenter_audio = restart_presenter_audio(
                            client,
                            path,
                            audio_recovery.clone(),
                            audio,
                            target_pts,
                            epoch,
                        );
                    }
                    prime_video_seek(client, &video_track, target_pts);
                    local_audio = None;
                    awaiting_keyframe = true;
                    recovery_rebase_pending = false;
                    recovery_start_pts_us = None;
                    started = false;
                    first_pts = None;
                    last_pts = None;
                    play_start_override = Some(target_pts);
                    playback_base_us = target;
                    playback_started_at = None;
                    ui.set_position_us(target);
                    ui.set_message(format!("Seek {}", target / 1_000_000));
                    ui.redraw()?;
                    continue;
                }
            }
            if let Some(recovery) = take_video_recovery(&video_channel)? {
                awaiting_keyframe = true;
                recovery_rebase_pending = started;
                apply_video_recovery(
                    client,
                    &video_track,
                    &mut video_channel,
                    recovery,
                    &mut epoch,
                    started,
                )?;
            }
            if packet.data.is_empty() || (awaiting_keyframe && !packet.key) {
                continue;
            }
            if awaiting_keyframe {
                awaiting_keyframe = false;
            }
            if recovery_rebase_pending {
                // A replacement decoder starts at this random-access unit, while linked audio keeps
                // its original timestamp domain. Fast-forward that audio worker to the same point and
                // publish the new authoritative clock before the keyframe, so a nested presenter does
                // not wait in real time for skipped audio or hold the frame forever behind PLAY(0).
                let start_pts_us = recovery_start_pts_us.take().unwrap_or(packet.pts_us);
                audio_recovery.request(start_pts_us);
                client.play(&video_track, start_pts_us, 1, MAXIMUM_LATENCY_US)?;
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
                    duration_us: packet.duration_us,
                    key: packet.key,
                    data: &packet.data,
                })
            } else {
                let channel = video_channel.clone();
                let data = packet.data.clone();
                let pts_us = packet.pts_us;
                let dts_us = packet.dts_us;
                let key = packet.key;
                let duration_us = packet.duration_us;
                // A seek's pre-roll is every access unit between the preceding random-access point
                // and the target, and none of it can be shown. Stream it: the presenter already
                // holds the exact target, discards those pictures without ever waiting on a clock,
                // and returns capacity as it goes, so a barrier per record would buy nothing and
                // cost a decode round trip on each one — the whole key-frame interval, serialized.
                // Initial pre-roll keeps the barrier, where one record is all that is wanted
                // before OUTPUT_READY and the presenter may hold the next output for PLAY.
                let barrier = play_start_override.is_none();
                let sender = thread::spawn(move || {
                    let sequence = channel.send_video(VideoPacket {
                        epoch,
                        packet_id,
                        pts_us,
                        dts_us,
                        duration_us,
                        key,
                        data: &data,
                    })?;
                    if barrier {
                        channel.wait_for_reusable_media_capacity()?;
                    }
                    Ok(sequence)
                });
                let (result, started_while_sending) =
                    finish_send_while_observing_start(sender, || {
                        if !readiness.due() {
                            return Ok(false);
                        }
                        let output_ready = client.query_track(&video_track)?.milestones
                            & MILESTONE_OUTPUT_READY
                            != 0;
                        if output_ready {
                            start_video_playback(
                                config,
                                client,
                                &surface,
                                &video_track,
                                &mut presenter_audio,
                                &mut local_audio,
                                play_start_override.unwrap_or_else(|| first_pts.unwrap_or(pts_us)),
                            )?;
                        }
                        Ok(output_ready)
                    })?;
                started = started_while_sending;
                if started {
                    playback_started_at.get_or_insert_with(Instant::now);
                }
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
                epoch = epoch
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("video epoch exhausted"))?;
                replace_video_channel(client, &video_track, &mut video_channel, epoch, started, 3)?;
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

            let output_ready = !started
                && readiness.due()
                && client.query_track(&video_track)?.milestones & MILESTONE_OUTPUT_READY != 0;
            if output_ready {
                start_video_playback(
                    config,
                    client,
                    &surface,
                    &video_track,
                    &mut presenter_audio,
                    &mut local_audio,
                    play_start_override.unwrap_or_else(|| first_pts.unwrap_or(packet.pts_us)),
                )?;
                started = true;
                playback_started_at.get_or_insert_with(Instant::now);
            }
        }
        if packet_id == 0 {
            return Err(
                io::Error::new(io::ErrorKind::UnexpectedEof, "video has no keyframes").into(),
            );
        }
        if packet_id == packets_before_generation && play_start_override.is_some() {
            cancel_presenter_audio(client, &mut presenter_audio);
            break 'playback;
        }
        if !started
            && play_start_override.is_some()
            && !video_output_ready(client, &video_track, SEEK_OUTPUT_GRACE)?
        {
            // The seek landed past the last decodable picture: the presenter discards every
            // output before the exact target, so the remaining packets decode to nothing it can
            // show and no priming frame is ever coming. That is the end of the file, not a start
            // to wait thirty seconds for behind a surface the FLUSH already cleared.
            cancel_presenter_audio(client, &mut presenter_audio);
            break 'playback;
        }
        if !started {
            start_video_playback(
                config,
                client,
                &surface,
                &video_track,
                &mut presenter_audio,
                &mut local_audio,
                play_start_override.unwrap_or_else(|| first_pts.unwrap_or(0)),
            )?;
            started = true;
            playback_started_at.get_or_insert_with(Instant::now);
        }

        let audio_wait_event = if let Some(audio) = presenter_audio.as_ref() {
            let audio_track = audio.track.clone();
            wait_for_worker_or_event(&audio.worker, || {
                if let Some(recovery) = take_video_recovery(&video_channel)? {
                    return Ok(Some(AudioWaitEvent::Recovery(recovery)));
                }
                let Some(ui) = ui.as_ref() else {
                    return Ok(None);
                };
                let current_us = playback_base_us.saturating_add(
                    playback_started_at
                        .map(|started| {
                            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
                        })
                        .unwrap_or(0),
                );
                ui.set_position_us(current_us.min(info.duration_us.unwrap_or(u64::MAX)));
                while let Some(command) = ui.try_command() {
                    match command {
                        Command::Quit => return Ok(Some(AudioWaitEvent::Quit)),
                        Command::SeekBy(delta) => {
                            let target = if delta < 0 {
                                current_us.saturating_sub(delta.unsigned_abs())
                            } else {
                                current_us.saturating_add(delta as u64)
                            };
                            return Ok(Some(AudioWaitEvent::Seek(target)));
                        }
                        Command::SeekTo(target) => {
                            return Ok(Some(AudioWaitEvent::Seek(target)));
                        }
                        Command::Resize(geometry) => {
                            let layout_geometry = media_geometry(geometry, true);
                            let size = display_size(
                                info.width,
                                info.height,
                                info.sar_num,
                                info.sar_den,
                                config.zoom,
                                layout_geometry,
                            );
                            (column, row) = centered_origin(layout_geometry, size.0, size.1);
                            update_full_window_surface(
                                client,
                                &mut placed_node,
                                column,
                                row,
                                size.0,
                                size.1,
                            )?;
                            (columns, rows) = size;
                            ui.redraw()?;
                        }
                        Command::VolumeBy(delta) => {
                            let next = (volume_percent as i32 + delta).clamp(0, 200) as u32;
                            if client.supports(vivid_protocol::registry::AUDIO_GAIN) {
                                client.set_audio_gain(
                                    &audio_track,
                                    vivid_sdk::AudioGain::from_percent(next)
                                        .expect("clamped volume is valid"),
                                )?;
                                volume_percent = next;
                                ui.set_volume_percent(Some(next));
                                ui.set_message(format!("Volume {next}%"));
                            } else {
                                ui.set_volume_percent(None);
                                ui.set_message("Volume unavailable");
                            }
                            ui.redraw()?;
                        }
                    }
                }
                Ok(None)
            })?
        } else {
            take_video_recovery(&video_channel)?.map(AudioWaitEvent::Recovery)
        };
        match audio_wait_event {
            Some(AudioWaitEvent::Recovery(recovery)) => {
                // The video demuxer can reach EOF while linked audio is still flow-controlled. A
                // tab projection change may then require a replacement decoder keyframe. Never
                // join the audio worker while that request is pending: the linked audio path is
                // intentionally gated until the keyframe arrives, so joining here would deadlock
                // both tracks and the playback UI. Seek to the last audio boundary already
                // accepted by the presenter and use the preceding random-access unit as pre-roll.
                let resume_pts_us = presenter_audio
                    .as_ref()
                    .and_then(|audio| audio.progress.snapshot().last_submitted_end_pts_us)
                    .or(first_pts)
                    .unwrap_or(timeline_origin_us);
                apply_video_recovery(
                    client,
                    &video_track,
                    &mut video_channel,
                    recovery,
                    &mut epoch,
                    started,
                )?;
                demuxer.seek_to_us(resume_pts_us)?;
                awaiting_keyframe = true;
                recovery_rebase_pending = started;
                recovery_start_pts_us = Some(resume_pts_us);
                continue 'playback;
            }
            Some(AudioWaitEvent::Quit) => {
                let _ = video_channel.close();
                cancel_presenter_audio(client, &mut presenter_audio);
                cleanup_full_window(client, &surface, &video_track, placed_node.node_id);
                return Ok(());
            }
            Some(AudioWaitEvent::Seek(target)) => {
                let target = target.min(info.duration_us.unwrap_or(u64::MAX));
                let target_pts =
                    timeline_origin_us.saturating_add(i64::try_from(target).unwrap_or(i64::MAX));
                client.pause(&video_track)?;
                epoch = epoch
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("video epoch exhausted"))?;
                replace_video_channel(client, &video_track, &mut video_channel, epoch, false, 1)?;
                demuxer.seek_to_us(target_pts)?;
                clear_audio_slot(client, &surface)?;
                let _ = audio_recovery.take();
                if let Some(audio) = presenter_audio.take() {
                    presenter_audio = restart_presenter_audio(
                        client,
                        path,
                        audio_recovery.clone(),
                        audio,
                        target_pts,
                        epoch,
                    );
                }
                prime_video_seek(client, &video_track, target_pts);
                local_audio = None;
                awaiting_keyframe = true;
                recovery_rebase_pending = false;
                recovery_start_pts_us = None;
                started = false;
                first_pts = None;
                last_pts = None;
                play_start_override = Some(target_pts);
                playback_base_us = target;
                playback_started_at = None;
                if let Some(ui) = ui.as_ref() {
                    ui.set_position_us(target);
                    ui.set_message(format!("Seek {}", target / 1_000_000));
                    ui.redraw()?;
                }
                continue 'playback;
            }
            None => {}
        }

        let mut audio_to_drain = None;
        let mut presenter_audio_failed = false;
        if let Some(audio) = presenter_audio.take() {
            match audio.worker.join() {
                Ok(outcome) if outcome.error.is_none() => {
                    outcome.channel.eos()?;
                    audio_to_drain = Some(DrainedPresenterAudio {
                        track: audio.track,
                        packet_ids: audio.packet_ids,
                    });
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
        if ui.is_none()
            && let Some(audio) = audio_to_drain.as_ref()
            && let Err(error) = client.drain(&audio.track)
        {
            client.verbose(format_args!("presenter audio drain failed: {error}"));
        }
        if config.no_wait {
            break 'playback;
        }
        let timeline = last_pts
            .zip(first_pts)
            .map_or(0, |(last, first)| last.saturating_sub(first).max(0) as u64);
        let timeout = Duration::from_micros(timeline).saturating_add(PLAYBACK_COMPLETION_GRACE);
        let outcome = wait_for_playback_end(
            client,
            &video_track,
            audio_to_drain.as_ref().map(|audio| &audio.track),
            ui.as_ref(),
            &mut volume_percent,
            playback_base_us,
            playback_started_at,
            info.duration_us,
            &info,
            config.zoom,
            &mut placed_node,
            &mut column,
            &mut row,
            &mut columns,
            &mut rows,
            timeout,
        )?;
        match outcome {
            WaitOutcome::Ended => {
                if let Some(audio) = audio_to_drain.as_ref()
                    && ui.is_some()
                    && let Err(error) = client.drain(&audio.track)
                {
                    client.verbose(format_args!("presenter audio drain failed: {error}"));
                }
                if let Some(audio) = local_audio.as_mut()
                    && let Err(error) = audio.wait()
                {
                    client.verbose(format_args!(
                        "local audio completion failed for {}: {error}",
                        path.display()
                    ));
                }
                break 'playback;
            }
            WaitOutcome::Quit => {
                break 'playback;
            }
            WaitOutcome::Seek(target) => {
                let retained_audio_track = if audio_to_drain.is_some() {
                    clear_audio_slot(client, &surface)?;
                    audio_to_drain.take()
                } else {
                    None
                };
                let target = target.min(info.duration_us.unwrap_or(u64::MAX));
                let target_pts =
                    timeline_origin_us.saturating_add(i64::try_from(target).unwrap_or(i64::MAX));
                epoch = epoch
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("video epoch exhausted"))?;
                client.pause(&video_track)?;
                replace_video_channel(client, &video_track, &mut video_channel, epoch, false, 1)?;
                demuxer.seek_to_us(target_pts)?;
                let _ = audio_recovery.take();
                presenter_audio = retained_audio_track.and_then(|audio| {
                    match restart_presenter_audio_track(
                        client,
                        path,
                        audio_recovery.clone(),
                        audio.track.clone(),
                        audio.packet_ids.clone(),
                        target_pts,
                        epoch,
                    ) {
                        Ok(audio) => Some(audio),
                        Err(error) => {
                            client.verbose(format_args!(
                                "presenter audio track {} could not advance for seek ({error}); continuing with video",
                                audio.track.id()
                            ));
                            None
                        }
                    }
                });
                prime_video_seek(client, &video_track, target_pts);
                local_audio = None;
                awaiting_keyframe = true;
                recovery_rebase_pending = false;
                recovery_start_pts_us = None;
                started = false;
                first_pts = None;
                last_pts = None;
                play_start_override = Some(target_pts);
                playback_base_us = target;
                playback_started_at = None;
                if let Some(ui) = ui.as_ref() {
                    ui.set_position_us(target);
                    ui.set_message(format!("Seek {}", target / 1_000_000));
                    ui.redraw()?;
                }
                continue 'playback;
            }
        }
    }
    client.verbose(format_args!(
        "video surface {surface_id}, track {}: {packet_id} packets presented at {columns}x{rows} cells",
        video_track.id()
    ));
    if ui.is_some() {
        cleanup_full_window(client, &surface, &video_track, placed_node.node_id);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn wait_for_playback_end(
    client: &mut VividClient,
    track: &Track,
    audio: Option<&Track>,
    ui: Option<&PlaybackUi>,
    volume_percent: &mut u32,
    playback_base_us: u64,
    playback_started_at: Option<Instant>,
    duration_us: Option<u64>,
    info: &VideoInfo,
    zoom: f32,
    placed_node: &mut vivid_sdk::SceneNode,
    column: &mut u32,
    row: &mut u32,
    columns: &mut u32,
    rows: &mut u32,
    overall_timeout: Duration,
) -> io::Result<WaitOutcome> {
    let deadline = Instant::now()
        .checked_add(overall_timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "playback wait is too long"))?;
    loop {
        if let Some(ui) = ui {
            let current = playback_base_us.saturating_add(
                playback_started_at
                    .map(|started| u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX))
                    .unwrap_or(0),
            );
            ui.set_position_us(current.min(duration_us.unwrap_or(u64::MAX)));
            while let Some(command) = ui.try_command() {
                match command {
                    Command::Quit => return Ok(WaitOutcome::Quit),
                    Command::VolumeBy(delta) => {
                        let next = (*volume_percent as i32 + delta).clamp(0, 200) as u32;
                        if let Some(audio) = audio
                            && client.supports(vivid_protocol::registry::AUDIO_GAIN)
                        {
                            client.set_audio_gain(
                                audio,
                                vivid_sdk::AudioGain::from_percent(next)
                                    .expect("clamped volume is valid"),
                            )?;
                            *volume_percent = next;
                            ui.set_volume_percent(Some(next));
                            ui.set_message(format!("Volume {next}%"));
                        } else {
                            ui.set_volume_percent(None);
                            ui.set_message("Volume unavailable");
                        }
                    }
                    Command::SeekBy(delta) => {
                        let target = if delta < 0 {
                            current.saturating_sub(delta.unsigned_abs())
                        } else {
                            current.saturating_add(delta as u64)
                        };
                        return Ok(WaitOutcome::Seek(
                            target.min(duration_us.unwrap_or(u64::MAX)),
                        ));
                    }
                    Command::SeekTo(target) => {
                        return Ok(WaitOutcome::Seek(
                            target.min(duration_us.unwrap_or(u64::MAX)),
                        ));
                    }
                    Command::Resize(geometry) => {
                        let layout_geometry = media_geometry(geometry, true);
                        let size = display_size(
                            info.width,
                            info.height,
                            info.sar_num,
                            info.sar_den,
                            zoom,
                            layout_geometry,
                        );
                        (*column, *row) = centered_origin(layout_geometry, size.0, size.1);
                        update_full_window_surface(
                            client,
                            placed_node,
                            *column,
                            *row,
                            size.0,
                            size.1,
                        )?;
                        (*columns, *rows) = size;
                    }
                }
            }
            ui.redraw()?;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for Vivid playback to finish",
            ));
        }
        let request_timeout = remaining
            .min(Duration::from_micros(MAX_TRACK_WAIT_TIMEOUT_US))
            .min(if ui.is_some() {
                Duration::from_millis(50)
            } else {
                Duration::MAX
            });
        let request_timeout_us = timeout_us(request_timeout).max(1);
        match client.wait_track(
            track,
            TrackWaitCondition::PlaybackEnded,
            None,
            request_timeout_us,
        ) {
            Ok(_) => return Ok(WaitOutcome::Ended),
            Err(error)
                if presenter_code(&error) == Some(ERROR_TIMEOUT) && Instant::now() < deadline => {}
            Err(error) => return Err(error),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    Ended,
    Seek(u64),
    Quit,
}

fn cleanup_full_window(
    client: &mut VividClient,
    surface: &vivid_sdk::Surface,
    video_track: &Track,
    node_id: u64,
) {
    let context_id = client.info().root_context_id;
    let _ = client.delete_node(context_id, node_id, &RequestMetadata::default());
    let _ = client.destroy_track(video_track, &RequestMetadata::default());
    let _ = client.destroy_surface(surface, &RequestMetadata::default());
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
    recovery: Arc<AudioRecoveryTarget>,
    start_pts_us: Option<i64>,
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
    Ok(Some(spawn_presenter_audio(
        track,
        channel,
        Arc::new(AtomicU64::new(0)),
        path,
        recovery,
        start_pts_us,
        1,
    )))
}

fn spawn_presenter_audio(
    track: Track,
    channel: Arc<TrackChannel>,
    packet_ids: Arc<AtomicU64>,
    path: &Path,
    recovery: Arc<AudioRecoveryTarget>,
    start_pts_us: Option<i64>,
    epoch: u32,
) -> PresenterAudio {
    let progress = Arc::new(AudioProgress::default());
    let stop = Arc::new(AtomicBool::new(false));
    let worker_progress = progress.clone();
    let worker_channel = channel.clone();
    let worker_packet_ids = packet_ids.clone();
    let worker_stop = stop.clone();
    let path = path.to_path_buf();
    let worker = thread::spawn(move || {
        let result = stream_audio(
            &path,
            &worker_channel,
            AudioStreamState {
                packet_ids: &worker_packet_ids,
                stop: &worker_stop,
                progress: &worker_progress,
                recovery: &recovery,
            },
            start_pts_us,
            epoch,
        );
        worker_progress.finish(result.is_err());
        match result {
            Ok(packet_id) => AudioOutcome {
                channel: worker_channel,
                packet_id,
                error: None,
            },
            Err(error) => AudioOutcome {
                channel: worker_channel,
                packet_id: worker_packet_ids.load(Ordering::Relaxed),
                error: Some(error),
            },
        }
    });
    PresenterAudio {
        track,
        channel,
        packet_ids,
        stop,
        progress,
        worker,
    }
}

fn restart_presenter_audio(
    client: &mut VividClient,
    path: &Path,
    recovery: Arc<AudioRecoveryTarget>,
    audio: PresenterAudio,
    start_pts_us: i64,
    epoch: u32,
) -> Option<PresenterAudio> {
    let PresenterAudio {
        track,
        channel,
        packet_ids,
        stop,
        progress: _,
        worker,
    } = audio;

    // There must be exactly one audio producer at a time. Merely dropping the old JoinHandle
    // detached it, so rapid seeks accumulated workers that raced records from retired channel
    // generations against the replacement. Advance while the old transport is still owned, which
    // makes any in-flight record stale without losing the track, then wake and join that worker
    // before FLUSH/OPEN starts the next generation.
    stop.store(true, Ordering::Release);
    let advanced = client.advance_channel(&track, 1, &RequestMetadata::default());
    let _ = channel.close();
    let worker_result = worker.join();
    drop(channel);

    if worker_result.is_err() {
        client.verbose(format_args!(
            "presenter audio track {} worker panicked while retiring for seek",
            track.id()
        ));
    }
    let result = advanced.and_then(|_| {
        open_advanced_presenter_audio_channel(client, &track, epoch).map(|channel| {
            spawn_presenter_audio(
                track.clone(),
                channel,
                packet_ids,
                path,
                recovery,
                Some(start_pts_us),
                epoch,
            )
        })
    });
    match result {
        Ok(audio) => Some(audio),
        Err(error) => {
            client.verbose(format_args!(
                "presenter audio track {} could not advance for seek ({error}); continuing with video",
                track.id()
            ));
            None
        }
    }
}

fn restart_presenter_audio_track(
    client: &mut VividClient,
    path: &Path,
    recovery: Arc<AudioRecoveryTarget>,
    track: Track,
    packet_ids: Arc<AtomicU64>,
    start_pts_us: i64,
    epoch: u32,
) -> io::Result<PresenterAudio> {
    let channel = advance_presenter_audio_channel(client, &track, epoch)?;
    Ok(spawn_presenter_audio(
        track,
        channel,
        packet_ids,
        path,
        recovery,
        Some(start_pts_us),
        epoch,
    ))
}

fn advance_presenter_audio_channel(
    client: &mut vivid_sdk::Session,
    track: &Track,
    epoch: u32,
) -> io::Result<Arc<TrackChannel>> {
    client.advance_channel(track, 1, &RequestMetadata::default())?;
    open_advanced_presenter_audio_channel(client, track, epoch)
}

fn open_advanced_presenter_audio_channel(
    client: &mut vivid_sdk::Session,
    track: &Track,
    epoch: u32,
) -> io::Result<Arc<TrackChannel>> {
    client.flush(track, epoch)?;
    client.open_track_channel(track).map(Arc::new)
}

fn stream_audio(
    path: &Path,
    channel: &TrackChannel,
    state: AudioStreamState<'_>,
    start_pts_us: Option<i64>,
    epoch: u32,
) -> io::Result<u64> {
    let mut demuxer = AudioDemuxer::open(path)?;
    if let Some(start_pts_us) = start_pts_us {
        demuxer.seek_to_us(start_pts_us)?;
    }
    let mut recovery_target = None;
    while !state.stop.load(Ordering::Acquire) {
        let Some(packet) = demuxer.next_packet()? else {
            break;
        };
        if start_pts_us.is_some_and(|start| {
            packet
                .pts_us
                .saturating_add(i64::try_from(packet.duration_us).unwrap_or(i64::MAX))
                <= start
        }) {
            continue;
        }
        merge_recovery_target(&mut recovery_target, state.recovery.take());
        if !audio_packet_reaches_recovery_target(&mut recovery_target, packet.pts_us) {
            continue;
        }
        if packet.data.is_empty() {
            continue;
        }
        if state.stop.load(Ordering::Acquire) {
            break;
        }
        let packet_id = next_audio_packet_id(state.packet_ids)?;
        channel.send_audio(AudioPacket {
            epoch,
            packet_id,
            pts_us: packet.pts_us,
            dts_us: packet.dts_us,
            duration_us: packet.duration_us,
            trim_start_samples: packet.trim_start_samples,
            trim_end_samples: packet.trim_end_samples,
            data: &packet.data,
        })?;
        state.progress.observe(packet.pts_us, packet.duration_us);
    }
    Ok(state.packet_ids.load(Ordering::Relaxed))
}

fn next_audio_packet_id(packet_ids: &AtomicU64) -> io::Result<u64> {
    packet_ids
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
            last.checked_add(1)
        })
        .map(|last| last + 1)
        .map_err(|_| io::Error::other("audio packet ID space exhausted"))
}

fn destroyed_audio_track(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

fn unavailable_audio_lifecycle(lifecycle: u64) -> bool {
    matches!(lifecycle, 6 | 7)
}

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
        let audio_lost = destroyed_audio_track(&wait_error)
            || client
                .query_track(audio)
                .is_ok_and(|status| unavailable_audio_lifecycle(status.lifecycle));
        if audio_lost || presenter_code(&wait_error) == Some(ERROR_TIMEOUT) {
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
        let audio_lost = if audio_active && let Some(audio) = audio {
            destroyed_audio_track(&activation_error)
                || client
                    .query_track(audio)
                    .is_ok_and(|status| unavailable_audio_lifecycle(status.lifecycle))
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
    if let Err(play_error) = client.play(clock, start_pts_us, minimum_buffer_us, MAXIMUM_LATENCY_US)
    {
        let audio_lost = audio_active
            && audio.is_some_and(|audio| {
                destroyed_audio_track(&play_error)
                    || client
                        .query_track(audio)
                        .is_ok_and(|status| unavailable_audio_lifecycle(status.lifecycle))
            });
        if !audio_lost {
            return Err(play_error);
        }
        let audio = audio.expect("lost presenter audio track exists");
        client.verbose(format_args!(
            "presenter audio track {} was lost before PLAY ({play_error}); retrying video alone",
            audio.id()
        ));
        audio_active = false;
        bindings.truncate(1);
        bindings.push(SlotBinding {
            slot: SLOT_AUDIO,
            track_id: 0,
            expected_channel_generation: ChannelGeneration::ZERO,
            required_milestone: 0,
        });
        client.activate_tracks(surface, &bindings, &RequestMetadata::default())?;
        client.play(video, start_pts_us, minimum_buffer_us, MAXIMUM_LATENCY_US)?;
    }
    if !config.no_wait {
        client.wait_track(
            video,
            TrackWaitCondition::PlaybackStarted,
            None,
            timeout_us(PLAYBACK_START_TIMEOUT),
        )?;
    }
    Ok(audio_active)
}

/// How often pre-roll asks the control plane whether decoded output exists yet.
const READINESS_POLL: Duration = Duration::from_millis(4);

/// Rate limiter for the `OUTPUT_READY` question asked between pre-roll records.
///
/// Each answer is a control round trip, and pre-roll for a seek is hundreds of records. Asking per
/// record, and again every millisecond a record is in flight, spends more of the seek on the
/// control plane than on the decoding it is waiting for. The interval is far below the delay a
/// person can see, so nothing is lost by asking less often.
#[derive(Default)]
struct ReadinessPoll {
    last: Option<Instant>,
}

impl ReadinessPoll {
    fn due(&mut self) -> bool {
        if self
            .last
            .is_some_and(|last| last.elapsed() < READINESS_POLL)
        {
            return false;
        }
        self.last = Some(Instant::now());
        true
    }
}

/// Whether the presenter has produced decoded output for the current generation within `grace`.
fn video_output_ready(
    client: &mut VividClient,
    video: &Track,
    grace: Duration,
) -> io::Result<bool> {
    match client.wait_track(
        video,
        TrackWaitCondition::MilestoneSet,
        Some(MILESTONE_OUTPUT_READY),
        timeout_us(grace),
    ) {
        Ok(_) => Ok(true),
        Err(error) if presenter_code(&error) == Some(ERROR_TIMEOUT) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Publish the exact seek target to the presenter before decoder pre-roll, without starting time.
///
/// PLAY is the only way to say where a flushed timed track resumes, and the presenter needs that
/// before the pre-roll so it can decode the reference frames ahead of the target while discarding
/// their pictures. It must not also start running: linked audio is unbound and still restarting,
/// so a clock started here would advance in real time through the whole activation handshake and
/// leave picture that far ahead of the sound it belongs with. PAUSE freezes the clock exactly on
/// the target, so the presenter releases the target picture and holds everything after it until
/// activation issues the authoritative PLAY to video and audio together.
///
/// The target is an optimization, not a requirement: a presenter that refuses it — a surface whose
/// video slot was never activated, for instance — still gets the same PLAY at activation.
fn prime_video_seek(client: &mut vivid_sdk::Session, video: &Track, start_pts_us: i64) {
    if client
        .play(video, start_pts_us, 0, MAXIMUM_LATENCY_US)
        .is_ok()
    {
        let _ = client.pause(video);
    }
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
    audio.stop.store(true, Ordering::Release);
    let _ = audio.channel.close();
    if audio.worker.join().is_err() {
        client.verbose(format_args!(
            "presenter audio track {} worker panicked while stopping",
            audio.track.id()
        ));
    }
    if let Err(error) = client.destroy_track(&audio.track, &RequestMetadata::default()) {
        client.verbose(format_args!(
            "could not destroy unusable presenter audio track {}: {error}",
            audio.track.id()
        ));
    }
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

fn wait_for_worker_or_event<T, E, F>(
    worker: &thread::JoinHandle<T>,
    mut take_event: F,
) -> io::Result<Option<E>>
where
    F: FnMut() -> io::Result<Option<E>>,
{
    loop {
        // Prefer an already queued recovery or UI command over completion. This keeps the last
        // accepted video packet and CHANNEL_EOS ordered around a projection change instead of
        // racing the worker, and leaves the playback UI responsive throughout the drain.
        if let Some(event) = take_event()? {
            return Ok(Some(event));
        }
        if worker.is_finished() {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn apply_video_recovery(
    client: &mut vivid_sdk::Session,
    video_track: &Track,
    video_channel: &mut Arc<TrackChannel>,
    recovery: VideoRecovery,
    epoch: &mut u32,
    started: bool,
) -> io::Result<()> {
    if recovery.advance_epoch {
        *epoch = epoch
            .checked_add(1)
            .ok_or_else(|| io::Error::other("video epoch exhausted"))?
            .max(recovery.minimum_epoch);
        replace_video_channel(client, video_track, video_channel, *epoch, started, 3)?;
    } else {
        *epoch = (*epoch).max(recovery.minimum_epoch);
    }
    Ok(())
}

fn replace_video_channel(
    client: &mut vivid_sdk::Session,
    video_track: &Track,
    video_channel: &mut Arc<TrackChannel>,
    epoch: u32,
    started: bool,
    reason: u64,
) -> io::Result<()> {
    let mut replacement = None;
    for control in recovery_control_order(started) {
        match control {
            // Control and media use independent transports. Advancing first scopes every queued
            // old-epoch record to the retired generation; flushing the current generation first
            // lets such a record close that current channel and lose the whole track.
            RecoveryControl::Pause => client.pause(video_track)?,
            RecoveryControl::Advance => {
                client.advance_channel(video_track, reason, &RequestMetadata::default())?;
                let _ = video_channel.close();
            }
            RecoveryControl::Flush => client.flush(video_track, epoch)?,
            RecoveryControl::Open => {
                replacement = Some(Arc::new(client.open_track_channel(video_track)?));
            }
        }
    }
    let replacement = replacement
        .ok_or_else(|| io::Error::other("video recovery did not open a replacement channel"))?;
    let retired = std::mem::replace(video_channel, replacement);
    drop(retired);
    Ok(())
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
    let (maximum_records_per_second, maximum_encoded_bits_per_second) = catchup_delivery_rates(
        client,
        info.maximum_records_per_second.max(1),
        info.maximum_encoded_bits_per_second.max(1),
    );
    Ok(TrackConfiguration {
        context_id: surface.context_id(),
        surface_id: surface.id(),
        track_id: client.allocate_id()?,
        slot: SLOT_VIDEO,
        mode: TrackMode::Timed,
        lane: LaneClass::Bulk,
        maximum_record_body,
        maximum_rate_millihertz: info.maximum_rate_millihertz.max(1),
        maximum_encoded_bits_per_second,
        maximum_records_per_second,
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

fn media_geometry(mut geometry: TerminalGeometry, full_window: bool) -> TerminalGeometry {
    if full_window && geometry.rows >= 2 {
        geometry.rows -= 1;
    }
    geometry
}

fn centered_origin(geometry: TerminalGeometry, columns: u32, rows: u32) -> (u32, u32) {
    (
        u32::from(geometry.cols).saturating_sub(columns) / 2,
        u32::from(geometry.rows).saturating_sub(rows) / 2,
    )
}

fn timeout_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_audio_recovery_coalesces_and_skips_directly_to_the_video_pts() {
        let recovery = AudioRecoveryTarget::default();
        recovery.request(8_000_000);
        recovery.request(7_000_000);
        recovery.request(8_400_000);

        let mut target = None;
        merge_recovery_target(&mut target, recovery.take());
        assert_eq!(target, Some(8_400_000));
        assert!(!audio_packet_reaches_recovery_target(&mut target, i64::MIN));
        assert!(!audio_packet_reaches_recovery_target(
            &mut target,
            8_399_999
        ));
        assert!(audio_packet_reaches_recovery_target(&mut target, 8_400_000));
        assert_eq!(target, None);
        assert!(audio_packet_reaches_recovery_target(&mut target, 8_420_000));
    }

    #[test]
    fn decoder_recovery_advances_before_flushing_and_opening() {
        assert_eq!(
            recovery_control_order(true).collect::<Vec<_>>(),
            vec![
                RecoveryControl::Pause,
                RecoveryControl::Advance,
                RecoveryControl::Flush,
                RecoveryControl::Open,
            ]
        );
        assert_eq!(
            recovery_control_order(false).collect::<Vec<_>>(),
            vec![
                RecoveryControl::Advance,
                RecoveryControl::Flush,
                RecoveryControl::Open,
            ]
        );
    }

    #[test]
    fn display_size_accounts_for_sample_aspect_ratio() {
        let geometry = TerminalGeometry::with_cell_size(100, 40, 10, 20);
        assert_eq!(display_size(640, 360, 1, 1, 1.0, geometry), (64, 18));
        assert_eq!(display_size(320, 360, 2, 1, 1.0, geometry), (64, 18));
    }

    #[test]
    fn full_window_layout_reserves_status_row_and_centers_video() {
        let geometry = TerminalGeometry::with_cell_size(100, 40, 10, 20);
        let media = media_geometry(geometry, true);
        assert_eq!(media.rows, 39);
        assert_eq!(centered_origin(media, 64, 18), (18, 10));
        assert_eq!(media_geometry(geometry, false), geometry);
    }

    #[test]
    fn audio_prebuffer_wait_finishes_on_failure() {
        let progress = AudioProgress::default();
        progress.finish(true);
        assert!(progress.wait_for_prebuffer().failed);
    }

    #[test]
    fn audio_progress_records_the_absolute_recovery_boundary() {
        let progress = AudioProgress::default();
        progress.observe(4_000_000, 20_000);
        progress.observe(4_020_000, 20_000);

        let state = progress.snapshot();
        assert_eq!(state.buffered_us, 40_000);
        assert_eq!(state.last_submitted_end_pts_us, Some(4_040_000));
    }

    #[test]
    fn repeated_seeks_preserve_audio_gain_and_advance_before_flush() {
        use vivid_sdk::testing::{ROOT_SECRET_HEX, TestPresenter};

        let presenter = TestPresenter::start(80, 24).unwrap();
        let mut producer = vivid_sdk::ProducerConfig {
            endpoint_control: Some(presenter.endpoint().to_owned()),
            endpoint_realtime: Some(presenter.endpoint().to_owned()),
            endpoint_bulk: Some(presenter.endpoint().to_owned()),
            authentication: vivid_sdk::ProducerAuthentication::root_hex(ROOT_SECRET_HEX).unwrap(),
            ..Default::default()
        };
        producer
            .optional_profiles
            .push(vivid_protocol::registry::AUDIO_GAIN.into());
        producer.optional_profiles.sort();
        let mut session = vivid_sdk::Session::connect(producer).unwrap();
        let surface = session
            .create_surface(
                video_surface(
                    session.info().root_context_id,
                    41,
                    Path::new("seek-test.webm"),
                    1,
                    1,
                ),
                &RequestMetadata::default(),
            )
            .unwrap();
        let mut opus_head = b"OpusHead".to_vec();
        opus_head.extend_from_slice(&[1, 2, 0, 0, 0x80, 0xbb, 0, 0, 0, 0, 0]);
        let maximum_record_body = vivid_protocol::media::audio_body_len(4_096).unwrap();
        let track = session
            .create_track(
                TrackConfiguration {
                    context_id: surface.context_id(),
                    surface_id: surface.id(),
                    track_id: 42,
                    slot: SLOT_AUDIO,
                    mode: TrackMode::Timed,
                    lane: LaneClass::Realtime,
                    maximum_record_body,
                    maximum_rate_millihertz: 50_000,
                    maximum_encoded_bits_per_second: 512_000,
                    maximum_records_per_second: 50,
                    maximum_inflight_body_bytes: u64::from(maximum_record_body) * 4,
                    kind: KindConfiguration::Audio(vivid_sdk::AudioConfiguration {
                        codec: "opus".into(),
                        packetization: vivid_protocol::media::AUDIO_PACKETIZATION_OPUS.into(),
                        extradata: opus_head,
                        sample_rate: 48_000,
                        channels: 2,
                        channel_mask: 3,
                        maximum_access_unit_bytes: 4_096,
                        codec_string: Some("opus".into()),
                    }),
                    target_latency_us: 0,
                    maximum_latency_us: MAXIMUM_LATENCY_US,
                    retained_pixel_charge: 0,
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        let mut channel = Arc::new(session.open_track_channel(&track).unwrap());
        let packet_ids = AtomicU64::new(0);
        session
            .set_audio_gain(&track, vivid_sdk::AudioGain::from_percent(35).unwrap())
            .unwrap();
        channel
            .send_audio(AudioPacket {
                epoch: 1,
                packet_id: next_audio_packet_id(&packet_ids).unwrap(),
                pts_us: 0,
                dts_us: 0,
                duration_us: 20_000,
                trim_start_samples: 0,
                trim_end_samples: 0,
                data: &[0],
            })
            .unwrap();

        for epoch in 2..=7 {
            let replacement = advance_presenter_audio_channel(&mut session, &track, epoch).unwrap();
            let retired = std::mem::replace(&mut channel, replacement);
            retired.close().unwrap();
            drop(retired);
            channel
                .send_audio(AudioPacket {
                    epoch,
                    packet_id: next_audio_packet_id(&packet_ids).unwrap(),
                    pts_us: i64::from(epoch) * 20_000,
                    dts_us: i64::from(epoch) * 20_000,
                    duration_us: 20_000,
                    trim_start_samples: 0,
                    trim_end_samples: 0,
                    data: &[0],
                })
                .unwrap();
        }

        assert_eq!(track.id(), 42);
        assert_eq!(track.channel_generation(), ChannelGeneration::new(7));
        assert_eq!(packet_ids.load(Ordering::Relaxed), 7);

        let maximum_video_body = vivid_protocol::media::video_body_len(16).unwrap();
        let video = session
            .create_track(
                TrackConfiguration {
                    context_id: surface.context_id(),
                    surface_id: surface.id(),
                    track_id: 43,
                    slot: SLOT_VIDEO,
                    mode: TrackMode::Timed,
                    lane: LaneClass::Bulk,
                    maximum_record_body: maximum_video_body,
                    maximum_rate_millihertz: 30_000,
                    maximum_encoded_bits_per_second: 1_000_000,
                    maximum_records_per_second: 30,
                    maximum_inflight_body_bytes: u64::from(maximum_video_body) * 2,
                    kind: KindConfiguration::Video(VideoConfiguration {
                        codec: "av1".into(),
                        packetization: "av1-low-overhead-tu-v1".into(),
                        extradata: vec![],
                        coded_width: 1,
                        coded_height: 1,
                        profile: 0,
                        level: 0,
                        maximum_reorder_depth: 1,
                        color_primaries: 1,
                        transfer: 1,
                        matrix: 0,
                        signal_range: 1,
                        aspect_numerator: 1,
                        aspect_denominator: 1,
                        maximum_access_unit_bytes: 16,
                        codec_string: None,
                        decoder_configuration: None,
                    }),
                    target_latency_us: 0,
                    maximum_latency_us: MAXIMUM_LATENCY_US,
                    retained_pixel_charge: 1,
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        let mut video_channel = Arc::new(session.open_track_channel(&video).unwrap());
        replace_video_channel(&mut session, &video, &mut video_channel, 2, false, 1).unwrap();
        prime_video_seek(&mut session, &video, 7_000_000);

        let observed = presenter.observed();
        assert_eq!(
            observed
                .iter()
                .filter(|request| {
                    request.object_id == track.id()
                        && request.record_type == vivid_protocol::messages::CREATE_TRACK
                })
                .count(),
            1
        );
        assert_eq!(
            observed
                .iter()
                .filter(|request| {
                    request.object_id == track.id()
                        && request.record_type == vivid_protocol::messages::ADVANCE_CHANNEL
                })
                .count(),
            6
        );
        assert_eq!(
            observed
                .iter()
                .filter(|request| {
                    request.object_id == track.id()
                        && request.record_type == vivid_protocol::messages::SET_AUDIO_GAIN
                })
                .count(),
            1
        );
        assert_eq!(
            observed
                .iter()
                .filter(|request| {
                    request.object_id == video.id()
                        && matches!(
                            request.record_type,
                            vivid_protocol::messages::ADVANCE_CHANNEL
                                | vivid_protocol::messages::FLUSH
                                | vivid_protocol::messages::PLAY
                                | vivid_protocol::messages::PAUSE
                        )
                })
                .map(|request| request.record_type)
                .collect::<Vec<_>>(),
            vec![
                vivid_protocol::messages::ADVANCE_CHANNEL,
                vivid_protocol::messages::FLUSH,
                vivid_protocol::messages::PLAY,
                vivid_protocol::messages::PAUSE,
            ],
            "a seek publishes its target and immediately freezes that clock on it"
        );
        let seek_play = observed
            .iter()
            .find(|request| {
                request.object_id == video.id()
                    && request.record_type == vivid_protocol::messages::PLAY
            })
            .unwrap();
        assert_eq!(
            seek_play
                .payload
                .iter()
                .find(|(key, _)| *key == 3)
                .and_then(|(_, value)| value.as_i64()),
            Some(7_000_000)
        );
        assert!(presenter.destroys().is_empty());
        drop(video_channel);
        drop(channel);
        session.close().unwrap();
    }

    #[test]
    fn destroyed_replacement_audio_is_treated_as_optional_track_loss() {
        let destroyed = io::Error::new(io::ErrorKind::NotFound, "track is destroyed");
        assert!(destroyed_audio_track(&destroyed));
        assert!(!destroyed_audio_track(&io::Error::other("control failed")));
        assert!(unavailable_audio_lifecycle(6));
        assert!(unavailable_audio_lifecycle(7));
        assert!(!unavailable_audio_lifecycle(1));
    }

    #[test]
    fn audio_completion_wait_keeps_servicing_recovery_and_ui_events() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = gate.clone();
        let worker = thread::spawn(move || {
            let (lock, changed) = &*worker_gate;
            let released = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let _released = changed
                .wait_while(released, |released| !*released)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        });
        let mut polls = 0;
        let event = wait_for_worker_or_event(&worker, || {
            polls += 1;
            Ok((polls == 2).then_some(7_u64))
        })
        .unwrap();
        assert_eq!(event, Some(7));
        assert!(!worker.is_finished());

        let (lock, changed) = &*gate;
        *lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        changed.notify_all();
        worker.join().unwrap();
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
