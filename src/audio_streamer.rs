use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use vivid_protocol::media::AudioPacket;
use vivid_protocol::messages::{ERROR_TIMEOUT, LaneClass};
use vivid_protocol::track::{KindConfiguration, TrackConfiguration, TrackMode};
use vivid_sdk::{
    AudioConfiguration, CoordinateModel, MILESTONE_OUTPUT_READY, RequestMetadata, SceneNode,
    SessionEvent, SlotBinding, SurfaceDefinition, SurfaceDescriptor, SurfaceRole, Track,
    TrackWaitCondition,
};

use crate::cli::Config;
use crate::client::{VividClient, catchup_delivery_rates};
use crate::ffmpeg::{AudioDemuxer, AudioInfo};
use crate::image_viewer::{raster_track_configuration, send_full_raster_frame};
use crate::playback_ui::{Command, PlaybackUi};
use crate::terminal_geometry::{
    TerminalGeometry, place_full_window_surface, place_surface, reserve_rows,
    resize_placed_surface, update_full_window_surface,
};
use crate::video_player::{centered_origin, display_size, media_geometry};

const INITIAL_BUFFER_US: u64 = 100_000;
const MAXIMUM_LATENCY_US: u64 = 2_000_000;
const PLAYBACK_TIMEOUT: Duration = Duration::from_secs(30);
const SLOT_AUDIO: u64 = 2;
/// Blank frame dimensions. The pane placement scales the surface (`Fit::Contain`) and a solid
/// frame has no detail to lose, so a small 16:9 raster keeps the Bulk transfer cheap.
const BLANK_WIDTH: u32 = 320;
const BLANK_HEIGHT: u32 = 180;
const UI_POLL_TIMEOUT_US: u64 = 50_000;

pub fn play(
    config: &Config,
    client: &mut VividClient,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let info = AudioDemuxer::inspect(path)?;
    let gain_available = client.supports(vivid_protocol::registry::AUDIO_GAIN);
    let ui = PlaybackUi::enter(config, path, info.duration_us, gain_available, true)?;

    let geometry = TerminalGeometry::settled_presenter(client);
    let layout = media_geometry(geometry, ui.is_some());
    let (columns, rows) = display_size(BLANK_WIDTH, BLANK_HEIGHT, 1, 1, config.zoom, layout);
    let surface_id = client.allocate_id()?;
    let node_id = client.allocate_id()?;
    let context_id = client.info().root_context_id;
    let surface = client.create_surface(
        audio_surface(context_id, surface_id, path, BLANK_WIDTH, BLANK_HEIGHT),
        &RequestMetadata::default(),
    )?;
    let (column, row) = centered_origin(layout, columns, rows);
    let placed_node = if ui.is_some() {
        place_full_window_surface(client, &surface, node_id, column, row, columns, rows)
    } else {
        place_surface(client, &surface, node_id, columns, rows)
    };
    let placed_node = match placed_node {
        Ok(node) => node,
        Err(error) => {
            let _ = client.destroy_surface(&surface, &RequestMetadata::default());
            return Err(error.into());
        }
    };
    if !config.is_dry_run() && ui.is_none() {
        reserve_rows(rows)?;
    }
    let mut pane = BlankPane {
        node: placed_node,
        column,
        row,
        columns,
        rows,
        full_window: ui.is_some(),
    };

    // The blank frame is presentation only: a presenter that refuses the raster track still
    // plays the audio instead of failing the whole file.
    let mut blank = match blank_track(client, &surface) {
        Ok(track) => Some(track),
        Err(error) => {
            client.verbose(format_args!(
                "audio {}: blank frame unavailable on this presenter ({error}); continuing without a pane",
                path.display()
            ));
            None
        }
    };

    let track = match setup_audio_track(client, &surface, &info) {
        Ok(track) => track,
        Err(error) => {
            teardown_audio_surface(client, &surface, None, blank.as_ref(), node_id);
            return Err(error.into());
        }
    };
    let result = stream_with_controls(
        config,
        client,
        path,
        &info,
        &surface,
        &track,
        &mut blank,
        &mut pane,
        ui.as_ref(),
    );
    teardown_audio_surface(client, &surface, Some(&track), blank.as_ref(), node_id);
    result?;
    Ok(())
}

/// The placed blank pane. The surface, node, and raster track are created once and survive every
/// seek generation: a seek freezes the blank frame while audio re-prebuffers.
struct BlankPane {
    node: SceneNode,
    column: u32,
    row: u32,
    columns: u32,
    rows: u32,
    full_window: bool,
}

impl BlankPane {
    fn resize(
        &mut self,
        client: &mut VividClient,
        geometry: TerminalGeometry,
        zoom: f32,
    ) -> io::Result<()> {
        let layout = media_geometry(geometry, self.full_window);
        let size = display_size(BLANK_WIDTH, BLANK_HEIGHT, 1, 1, zoom, layout);
        if size == (self.columns, self.rows) {
            return Ok(());
        }
        if self.full_window {
            (self.column, self.row) = centered_origin(layout, size.0, size.1);
            update_full_window_surface(
                client,
                &mut self.node,
                self.column,
                self.row,
                size.0,
                size.1,
            )?;
        } else {
            resize_placed_surface(client, &mut self.node, size.0, size.1)?;
        }
        (self.columns, self.rows) = size;
        Ok(())
    }
}

/// Creates the blank still-frame raster track and submits its single opaque frame, waiting for
/// the presenter to publish it before activation.
fn blank_track(client: &mut vivid_sdk::Session, surface: &vivid_sdk::Surface) -> io::Result<Track> {
    let configuration = raster_track_configuration(client, surface, BLANK_WIDTH, BLANK_HEIGHT)?;
    let mut probe = configuration.clone();
    probe.track_id = 0;
    if !client.probe_track(&probe)?.supported {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "presenter rejected the blank raster track configuration",
        ));
    }
    let track = client.create_track(configuration, &RequestMetadata::default())?;
    send_full_raster_frame(client, &track, &blank_frame(BLANK_WIDTH, BLANK_HEIGHT)?)?;
    client.wait_track(
        &track,
        TrackWaitCondition::MilestoneSet,
        Some(MILESTONE_OUTPUT_READY),
        timeout_us(PLAYBACK_TIMEOUT),
    )?;
    Ok(track)
}

/// Straight-alpha opaque black frame: RGB zero, alpha 255.
fn blank_frame(width: u32, height: u32) -> io::Result<Vec<u8>> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "blank frame size overflow"))?;
    let mut rgba = vec![0_u8; pixels];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    Ok(rgba)
}

fn setup_audio_track(
    client: &mut vivid_sdk::Session,
    surface: &vivid_sdk::Surface,
    info: &AudioInfo,
) -> io::Result<Track> {
    let configuration = audio_track(client, surface, info)?;
    let mut probe = configuration.clone();
    probe.track_id = 0;
    if !client.probe_track(&probe)?.supported {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "presenter rejected the audio track configuration",
        ));
    }
    client.create_track(configuration, &RequestMetadata::default())
}

/// Best-effort teardown in the video player's order: scene node, tracks, surface.
fn teardown_audio_surface(
    client: &mut vivid_sdk::Session,
    surface: &vivid_sdk::Surface,
    audio: Option<&Track>,
    blank: Option<&Track>,
    node_id: u64,
) {
    let context_id = client.info().root_context_id;
    let _ = client.delete_node(context_id, node_id, &RequestMetadata::default());
    if let Some(audio) = audio {
        let _ = client.destroy_track(audio, &RequestMetadata::default());
    }
    if let Some(blank) = blank {
        let _ = client.destroy_track(blank, &RequestMetadata::default());
    }
    let _ = client.destroy_surface(surface, &RequestMetadata::default());
}

#[allow(clippy::too_many_arguments)]
fn stream_with_controls(
    config: &Config,
    client: &mut VividClient,
    path: &Path,
    info: &AudioInfo,
    surface: &vivid_sdk::Surface,
    track: &Track,
    blank: &mut Option<Track>,
    pane: &mut BlankPane,
    ui: Option<&PlaybackUi>,
) -> Result<(), Box<dyn std::error::Error>> {
    let origin_us = info.first_pts_us.unwrap_or(0);
    let mut elapsed_us = 0_u64;
    let mut epoch = 1_u32;
    let mut packet_id = 0_u64;
    let mut volume_percent = 100_u32;
    let mut generation = 0_u64;

    'generation: loop {
        if generation != 0 {
            epoch = epoch
                .checked_add(1)
                .ok_or_else(|| io::Error::other("audio epoch exhausted"))?;
            client.pause(track)?;
            client.flush(track, epoch)?;
            client.advance_channel(track, 1, &RequestMetadata::default())?;
        }
        generation = generation.saturating_add(1);
        let channel = client.open_track_channel(track)?;
        let target_pts_us = origin_us.saturating_add(i64::try_from(elapsed_us).unwrap_or(i64::MAX));
        let mut demuxer = AudioDemuxer::open(path)?;
        if elapsed_us != 0 {
            demuxer.seek_to_us(target_pts_us)?;
        }
        let mut started = false;
        let mut buffered_us = 0_u64;
        let mut started_at = None;
        let mut packets_this_generation = 0_u64;

        while let Some(packet) = demuxer.next_packet()? {
            if let Some(geometry) = take_target_geometry(client, track.id())? {
                pane.resize(client, geometry, config.zoom)?;
            }
            if let Some(action) = handle_commands(
                client,
                track,
                ui,
                pane,
                config.zoom,
                &mut volume_percent,
                current_elapsed(elapsed_us, started_at),
                info.duration_us,
            )? {
                channel.close()?;
                match action {
                    ControlAction::Seek(target) => {
                        elapsed_us = target;
                        continue 'generation;
                    }
                    ControlAction::Quit => return Ok(()),
                }
            }
            if packet.data.is_empty()
                || packet
                    .pts_us
                    .saturating_add(i64::try_from(packet.duration_us).unwrap_or(i64::MAX))
                    <= target_pts_us
            {
                continue;
            }
            packet_id = packet_id
                .checked_add(1)
                .ok_or_else(|| io::Error::other("audio packet ID space exhausted"))?;
            packets_this_generation += 1;
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
            buffered_us = buffered_us.saturating_add(packet.duration_us);
            if !started && buffered_us >= INITIAL_BUFFER_US {
                start_playback(
                    client,
                    surface,
                    track,
                    blank,
                    target_pts_us,
                    INITIAL_BUFFER_US,
                )?;
                started = true;
                started_at = Some(Instant::now());
            }
            if let Some(ui) = ui {
                ui.set_position_us(current_elapsed(elapsed_us, started_at));
                ui.redraw()?;
            }
        }
        if packets_this_generation == 0 {
            if generation == 1 {
                return Err(
                    io::Error::new(io::ErrorKind::UnexpectedEof, "audio has no packets").into(),
                );
            }
            return Ok(());
        }
        if !started {
            start_playback(
                client,
                surface,
                track,
                blank,
                target_pts_us,
                buffered_us.max(1),
            )?;
            started_at = Some(Instant::now());
        }
        channel.eos()?;
        if config.no_wait {
            break;
        }
        if ui.is_none() {
            client.drain(track)?;
        }
        loop {
            let current = current_elapsed(elapsed_us, started_at);
            if let Some(ui) = ui {
                ui.set_position_us(current.min(info.duration_us.unwrap_or(u64::MAX)));
                ui.redraw()?;
            }
            if let Some(action) = handle_commands(
                client,
                track,
                ui,
                pane,
                config.zoom,
                &mut volume_percent,
                current,
                info.duration_us,
            )? {
                match action {
                    ControlAction::Seek(target) => {
                        elapsed_us = target;
                        continue 'generation;
                    }
                    ControlAction::Quit => return Ok(()),
                }
            }
            match client.wait_track(
                track,
                TrackWaitCondition::PlaybackEnded,
                None,
                UI_POLL_TIMEOUT_US,
            ) {
                Ok(_) => {
                    if ui.is_some() {
                        client.drain(track)?;
                    }
                    break 'generation;
                }
                Err(error) if presenter_code(&error) == Some(ERROR_TIMEOUT) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    client.verbose(format_args!(
        "audio track {}: EOS after {packet_id} packets",
        track.id()
    ));
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum ControlAction {
    Seek(u64),
    Quit,
}

#[allow(clippy::too_many_arguments)]
fn handle_commands(
    client: &mut VividClient,
    track: &Track,
    ui: Option<&PlaybackUi>,
    pane: &mut BlankPane,
    zoom: f32,
    volume_percent: &mut u32,
    current_us: u64,
    duration_us: Option<u64>,
) -> io::Result<Option<ControlAction>> {
    let Some(ui) = ui else { return Ok(None) };
    while let Some(command) = ui.try_command() {
        match command {
            Command::SeekBy(delta) => {
                let target = if delta < 0 {
                    current_us.saturating_sub(delta.unsigned_abs())
                } else {
                    current_us.saturating_add(delta as u64)
                };
                return Ok(Some(ControlAction::Seek(
                    target.min(duration_us.unwrap_or(u64::MAX)),
                )));
            }
            Command::SeekTo(target) => {
                return Ok(Some(ControlAction::Seek(
                    target.min(duration_us.unwrap_or(u64::MAX)),
                )));
            }
            Command::VolumeBy(delta) => {
                let next = (*volume_percent as i32 + delta).clamp(0, 200) as u32;
                if client.supports(vivid_protocol::registry::AUDIO_GAIN) {
                    let gain = vivid_sdk::AudioGain::from_percent(next).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "invalid volume")
                    })?;
                    client.set_audio_gain(track, gain)?;
                    *volume_percent = next;
                    ui.set_volume_percent(Some(next));
                    ui.set_message(format!("Volume {next}%"));
                } else {
                    ui.set_volume_percent(None);
                    ui.set_message("Volume unavailable on this presenter");
                }
                ui.redraw()?;
            }
            Command::Resize(geometry) => {
                pane.resize(client, geometry, zoom)?;
                ui.redraw()?;
            }
            Command::Quit => return Ok(Some(ControlAction::Quit)),
        }
    }
    Ok(None)
}

/// Drains presenter events, applying target changes and failing on connection or audio-track
/// loss, mirroring the video player's geometry event handling.
fn take_target_geometry(
    client: &mut VividClient,
    audio_track_id: u64,
) -> io::Result<Option<TerminalGeometry>> {
    let mut changed = false;
    while let Some(event) = client.take_event()? {
        match event {
            SessionEvent::TargetChanged(payload) => {
                client.apply_target_changed(&payload)?;
                changed = true;
            }
            SessionEvent::TrackLost { object_id, .. } if object_id == audio_track_id => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "presenter reported the audio track lost",
                ));
            }
            SessionEvent::ConnectionClosed { diagnostic } => {
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

fn current_elapsed(base_us: u64, started_at: Option<Instant>) -> u64 {
    base_us.saturating_add(
        started_at
            .map(|instant| u64::try_from(instant.elapsed().as_micros()).unwrap_or(u64::MAX))
            .unwrap_or(0),
    )
}

fn presenter_code(error: &io::Error) -> Option<u64> {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<vivid_sdk::PresenterError>())
        .map(|error| error.code)
}

fn start_playback(
    client: &mut vivid_sdk::Session,
    surface: &vivid_sdk::Surface,
    track: &Track,
    blank: &mut Option<Track>,
    start_pts_us: i64,
    minimum_buffer_us: u64,
) -> io::Result<()> {
    client.wait_track(
        track,
        TrackWaitCondition::MilestoneSet,
        Some(MILESTONE_OUTPUT_READY),
        timeout_us(PLAYBACK_TIMEOUT),
    )?;
    let audio_binding = || SlotBinding {
        slot: SLOT_AUDIO,
        track_id: track.id(),
        expected_channel_generation: track.channel_generation(),
        required_milestone: MILESTONE_OUTPUT_READY,
    };
    if let Some(blank_track) = blank.as_ref() {
        let joint = [
            SlotBinding {
                slot: blank_track.configuration()?.slot,
                track_id: blank_track.id(),
                expected_channel_generation: blank_track.channel_generation(),
                required_milestone: MILESTONE_OUTPUT_READY,
            },
            audio_binding(),
        ];
        match client.activate_tracks(surface, &joint, &RequestMetadata::default()) {
            Ok(_) => {}
            Err(error) => {
                // A dead raster track drops the pane and retries audio alone; anything else is a
                // real activation failure.
                let blank_lost = error.kind() == io::ErrorKind::NotFound
                    || client
                        .query_track(blank_track)
                        .is_ok_and(|status| matches!(status.lifecycle, 6 | 7));
                if !blank_lost {
                    return Err(error);
                }
                *blank = None;
                client.activate_tracks(surface, &[audio_binding()], &RequestMetadata::default())?;
            }
        }
    } else {
        client.activate_tracks(surface, &[audio_binding()], &RequestMetadata::default())?;
    }
    client.play(track, start_pts_us, minimum_buffer_us, MAXIMUM_LATENCY_US)?;
    client.wait_track(
        track,
        TrackWaitCondition::PlaybackStarted,
        None,
        timeout_us(PLAYBACK_TIMEOUT),
    )?;
    Ok(())
}

fn audio_surface(
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
                .unwrap_or("audio")
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

pub(crate) fn audio_track(
    client: &vivid_sdk::Session,
    surface: &vivid_sdk::Surface,
    info: &AudioInfo,
) -> io::Result<TrackConfiguration> {
    let maximum_record_body = vivid_protocol::media::audio_body_len(info.max_access_unit_bytes)
        .map_err(io::Error::other)?;
    let maximum_inflight_body_bytes = u64::from(maximum_record_body)
        .saturating_mul(8)
        .max(u64::from(maximum_record_body));
    let (maximum_records_per_second, maximum_encoded_bits_per_second) = catchup_delivery_rates(
        client,
        info.maximum_records_per_second.max(1),
        info.maximum_encoded_bits_per_second.max(1),
    );
    Ok(TrackConfiguration {
        context_id: surface.context_id(),
        surface_id: surface.id(),
        track_id: client.allocate_id()?,
        slot: SLOT_AUDIO,
        mode: TrackMode::Timed,
        lane: LaneClass::Realtime,
        maximum_record_body,
        maximum_rate_millihertz: info.maximum_rate_millihertz.max(1),
        maximum_encoded_bits_per_second,
        maximum_records_per_second,
        maximum_inflight_body_bytes,
        kind: KindConfiguration::Audio(AudioConfiguration {
            codec: info.codec.clone(),
            packetization: info.packetization.clone(),
            extradata: info.extradata.clone(),
            sample_rate: info.sample_rate,
            channels: u8::try_from(info.channels).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "audio channels exceed u8")
            })?,
            channel_mask: info.channel_mask,
            maximum_access_unit_bytes: info.max_access_unit_bytes,
            codec_string: info.codec_string.clone(),
        }),
        target_latency_us: 0,
        maximum_latency_us: MAXIMUM_LATENCY_US,
        retained_pixel_charge: 0,
    })
}

fn timeout_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use vivid_protocol::cbor::Value;
    use vivid_protocol::messages;
    use vivid_sdk::testing::{ROOT_SECRET_HEX, TestPresenter};

    use super::*;

    fn pane_producer(presenter: &TestPresenter) -> vivid_sdk::ProducerConfig {
        vivid_sdk::ProducerConfig {
            endpoint_control: Some(presenter.endpoint().to_owned()),
            endpoint_realtime: Some(presenter.endpoint().to_owned()),
            endpoint_bulk: Some(presenter.endpoint().to_owned()),
            authentication: vivid_sdk::ProducerAuthentication::root_hex(ROOT_SECRET_HEX).unwrap(),
            ..Default::default()
        }
    }

    fn write_pcm_wav(tag: &str) -> PathBuf {
        static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vivi-audio-pane-{tag}-{}-{sequence}.wav",
            std::process::id()
        ));
        fs::write(&path, crate::audio_player::pcm_wav()).unwrap();
        path
    }

    fn pane_packet(epoch: u32, packet_id: u64, pts_us: i64) -> AudioPacket<'static> {
        AudioPacket {
            epoch,
            packet_id,
            pts_us,
            dts_us: pts_us,
            duration_us: 20_000,
            trim_start_samples: 0,
            trim_end_samples: 0,
            data: &[0],
        }
    }

    fn fixed_blank_track(
        session: &mut vivid_sdk::Session,
        surface: &vivid_sdk::Surface,
        track_id: u64,
    ) -> io::Result<Track> {
        let mut configuration =
            raster_track_configuration(session, surface, BLANK_WIDTH, BLANK_HEIGHT)?;
        configuration.track_id = track_id;
        let track = session.create_track(configuration, &RequestMetadata::default())?;
        send_full_raster_frame(session, &track, &blank_frame(BLANK_WIDTH, BLANK_HEIGHT)?)?;
        Ok(track)
    }

    fn fixed_audio_track(
        session: &mut vivid_sdk::Session,
        surface: &vivid_sdk::Surface,
        info: &AudioInfo,
        track_id: u64,
    ) -> io::Result<Track> {
        let mut configuration = audio_track(session, surface, info)?;
        configuration.track_id = track_id;
        session.create_track(configuration, &RequestMetadata::default())
    }

    fn seek_generation(
        session: &mut vivid_sdk::Session,
        audio: &Track,
        channel: vivid_sdk::TrackChannel,
    ) -> io::Result<vivid_sdk::TrackChannel> {
        channel.close()?;
        drop(channel);
        session.pause(audio)?;
        session.flush(audio, 2)?;
        session.advance_channel(audio, 1, &RequestMetadata::default())?;
        session.open_track_channel(audio)
    }

    #[test]
    fn blank_pane_and_audio_activate_together_across_seek_generations() {
        let path = write_pcm_wav("generations");
        let presenter = TestPresenter::start(80, 24).unwrap();
        let mut session = vivid_sdk::Session::connect(pane_producer(&presenter)).unwrap();

        let info = AudioDemuxer::inspect(&path).unwrap();
        let surface = session
            .create_surface(
                audio_surface(
                    session.info().root_context_id,
                    41,
                    &path,
                    BLANK_WIDTH,
                    BLANK_HEIGHT,
                ),
                &RequestMetadata::default(),
            )
            .unwrap();
        let mut blank = Some(blank_track(&mut session, &surface).unwrap());
        let blank_id = blank.as_ref().unwrap().id();
        let audio = setup_audio_track(&mut session, &surface, &info).unwrap();
        let audio_id = audio.id();

        let channel = session.open_track_channel(&audio).unwrap();
        channel.send_audio(pane_packet(1, 1, 0)).unwrap();
        start_playback(
            &mut session,
            &surface,
            &audio,
            &mut blank,
            0,
            INITIAL_BUFFER_US,
        )
        .unwrap();

        let channel = seek_generation(&mut session, &audio, channel).unwrap();
        channel.send_audio(pane_packet(2, 2, 20_000)).unwrap();
        start_playback(
            &mut session,
            &surface,
            &audio,
            &mut blank,
            20_000,
            INITIAL_BUFFER_US,
        )
        .unwrap();
        assert!(blank.is_some(), "the blank pane survives seek generations");
        channel.eos().unwrap();
        drop(channel);

        teardown_audio_surface(&mut session, &surface, Some(&audio), blank.as_ref(), 44);

        let observed = presenter.observed();
        let count = |object_id: u64, record: u16| {
            observed
                .iter()
                .filter(|request| request.object_id == object_id && request.record_type == record)
                .count()
        };
        assert_eq!(count(blank_id, messages::CREATE_TRACK), 1);
        assert_eq!(count(blank_id, messages::ADVANCE_CHANNEL), 0);
        assert_eq!(
            count(blank_id, messages::PLAY),
            0,
            "the pane is never the clock"
        );
        assert_eq!(count(audio_id, messages::ADVANCE_CHANNEL), 1);

        let plays = observed
            .iter()
            .filter(|request| {
                request.object_id == audio_id && request.record_type == messages::PLAY
            })
            .map(|request| {
                request
                    .payload
                    .iter()
                    .find(|(key, _)| *key == 3)
                    .and_then(|(_, value)| value.as_i64())
            })
            .collect::<Vec<_>>();
        assert_eq!(plays, vec![Some(0), Some(20_000)]);

        let activations = observed
            .iter()
            .filter(|request| {
                request.record_type == messages::ACTIVATE_TRACK && request.object_id == 41
            })
            .collect::<Vec<_>>();
        assert_eq!(activations.len(), 2, "each generation activates once");
        for activation in activations {
            let bindings = activation
                .payload
                .iter()
                .find(|(key, _)| *key == 2)
                .and_then(|(_, value)| match value {
                    Value::Array(bindings) => Some(bindings.clone()),
                    _ => None,
                })
                .expect("activation bindings");
            let slots = bindings
                .iter()
                .map(|binding| match binding {
                    Value::Map(fields) => fields
                        .iter()
                        .find(|(key, _)| *key == 0)
                        .and_then(|(_, value)| value.as_u64()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                slots,
                vec![Some(3), Some(2)],
                "the blank pane and audio activate atomically together"
            );
        }

        let channels = presenter.channels();
        let blank_channel = channels
            .iter()
            .find(|log| log.track_id == blank_id)
            .expect("blank raster channel");
        assert_eq!(blank_channel.media_records, 1);
        assert!(
            channels
                .iter()
                .any(|log| log.track_id == audio_id && log.media_records >= 1)
        );

        let destroys = presenter.destroys();
        assert_eq!(
            destroys
                .iter()
                .map(|destroy| destroy.track_id)
                .collect::<Vec<_>>(),
            vec![audio_id, blank_id],
            "audio tears down before the blank pane"
        );
        assert!(
            destroys
                .iter()
                .find(|destroy| destroy.track_id == audio_id)
                .expect("audio destroy")
                .closed_before_destroy,
            "audio ends its channel with ordered EOS before the destroy"
        );

        let _ = fs::remove_file(&path);
        session.close().unwrap();
    }

    #[test]
    fn owner_isolation_survives_same_local_ids_across_owners() {
        let path = write_pcm_wav("owners");
        let info = AudioDemuxer::inspect(&path).unwrap();

        let presenter_a = TestPresenter::start(80, 24).unwrap();
        let mut session_a = vivid_sdk::Session::connect(pane_producer(&presenter_a)).unwrap();
        let surface_a = session_a
            .create_surface(
                audio_surface(
                    session_a.info().root_context_id,
                    41,
                    &path,
                    BLANK_WIDTH,
                    BLANK_HEIGHT,
                ),
                &RequestMetadata::default(),
            )
            .unwrap();
        let mut blank_a = Some(fixed_blank_track(&mut session_a, &surface_a, 42).unwrap());
        let audio_a = fixed_audio_track(&mut session_a, &surface_a, &info, 43).unwrap();
        let channel_a = session_a.open_track_channel(&audio_a).unwrap();
        channel_a.send_audio(pane_packet(1, 1, 0)).unwrap();
        start_playback(
            &mut session_a,
            &surface_a,
            &audio_a,
            &mut blank_a,
            0,
            INITIAL_BUFFER_US,
        )
        .unwrap();

        // Owner B reuses every local numeric ID owner A is using right now.
        let presenter_b = TestPresenter::start(80, 24).unwrap();
        let mut session_b = vivid_sdk::Session::connect(pane_producer(&presenter_b)).unwrap();
        let surface_b = session_b
            .create_surface(
                audio_surface(
                    session_b.info().root_context_id,
                    41,
                    &path,
                    BLANK_WIDTH,
                    BLANK_HEIGHT,
                ),
                &RequestMetadata::default(),
            )
            .unwrap();
        let mut blank_b = Some(fixed_blank_track(&mut session_b, &surface_b, 42).unwrap());
        let audio_b = fixed_audio_track(&mut session_b, &surface_b, &info, 43).unwrap();
        let channel_b = session_b.open_track_channel(&audio_b).unwrap();
        channel_b.send_audio(pane_packet(1, 1, 0)).unwrap();
        start_playback(
            &mut session_b,
            &surface_b,
            &audio_b,
            &mut blank_b,
            0,
            INITIAL_BUFFER_US,
        )
        .unwrap();

        // Tear owner A down completely while owner B keeps its pane and audio.
        channel_a.close().unwrap();
        drop(channel_a);
        teardown_audio_surface(
            &mut session_a,
            &surface_a,
            Some(&audio_a),
            blank_a.as_ref(),
            44,
        );
        session_a.close().unwrap();
        drop(presenter_a);

        let channel_b = seek_generation(&mut session_b, &audio_b, channel_b).unwrap();
        channel_b.send_audio(pane_packet(2, 2, 20_000)).unwrap();
        start_playback(
            &mut session_b,
            &surface_b,
            &audio_b,
            &mut blank_b,
            20_000,
            INITIAL_BUFFER_US,
        )
        .unwrap();
        assert!(
            blank_b.is_some(),
            "owner B keeps its pane after A's teardown"
        );

        let observed_b = presenter_b.observed();
        let count_b = |object_id: u64, record: u16| {
            observed_b
                .iter()
                .filter(|request| request.object_id == object_id && request.record_type == record)
                .count()
        };
        assert_eq!(count_b(41, messages::ACTIVATE_TRACK), 2);
        assert_eq!(count_b(42, messages::CREATE_TRACK), 1);
        assert_eq!(count_b(43, messages::ADVANCE_CHANNEL), 1);
        assert_eq!(count_b(43, messages::PLAY), 2);
        assert!(
            presenter_b.destroys().is_empty(),
            "owner A's teardown never reaches owner B's objects"
        );

        channel_b.eos().unwrap();
        drop(channel_b);
        teardown_audio_surface(
            &mut session_b,
            &surface_b,
            Some(&audio_b),
            blank_b.as_ref(),
            44,
        );
        session_b.close().unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn dry_run_audio_only_play_places_the_pane_and_terminates() {
        let path = write_pcm_wav("dry-run");
        let config = Config {
            files: vec![path.clone()],
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
        let mut client = VividClient::connect(&config).unwrap();
        play(&config, &mut client, &path).unwrap();
        client.close().unwrap();
        let _ = fs::remove_file(&path);
    }
}
