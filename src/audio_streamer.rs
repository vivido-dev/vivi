use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use vivid_protocol::media::AudioPacket;
use vivid_protocol::messages::{ERROR_TIMEOUT, LaneClass};
use vivid_protocol::track::{KindConfiguration, TrackConfiguration, TrackMode};
use vivid_sdk::{
    AudioConfiguration, CoordinateModel, MILESTONE_OUTPUT_READY, RequestMetadata, SlotBinding,
    SurfaceDefinition, SurfaceDescriptor, SurfaceRole, Track, TrackWaitCondition,
};

use crate::cli::Config;
use crate::client::{VividClient, catchup_delivery_rates};
use crate::ffmpeg::{AudioDemuxer, AudioInfo};
use crate::playback_ui::{Command, PlaybackUi};

const INITIAL_BUFFER_US: u64 = 100_000;
const MAXIMUM_LATENCY_US: u64 = 2_000_000;
const PLAYBACK_TIMEOUT: Duration = Duration::from_secs(30);
const SLOT_AUDIO: u64 = 2;
const UI_POLL_TIMEOUT_US: u64 = 50_000;

pub fn play(
    config: &Config,
    client: &mut VividClient,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let info = AudioDemuxer::inspect(path)?;
    let gain_available = client.supports(vivid_protocol::registry::AUDIO_GAIN);
    let ui = PlaybackUi::enter(config, path, info.duration_us, gain_available, true)?;
    let surface_id = client.allocate_id()?;
    let context_id = client.info().root_context_id;
    let surface = client.create_surface(
        audio_surface(context_id, surface_id, path),
        &RequestMetadata::default(),
    )?;
    let configuration = audio_track(client, &surface, &info)?;
    let mut probe = configuration.clone();
    probe.track_id = 0;
    if !client.probe_track(&probe)?.supported {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "presenter rejected the audio track configuration",
        )
        .into());
    }
    let track = client.create_track(configuration, &RequestMetadata::default())?;
    let result = stream_with_controls(config, client, path, &info, &surface, &track, ui.as_ref());
    let track_cleanup = client.destroy_track(&track, &RequestMetadata::default());
    let surface_cleanup = client.destroy_surface(&surface, &RequestMetadata::default());
    result?;
    track_cleanup?;
    surface_cleanup?;
    Ok(())
}

fn stream_with_controls(
    config: &Config,
    client: &mut VividClient,
    path: &Path,
    info: &AudioInfo,
    surface: &vivid_sdk::Surface,
    track: &Track,
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
            if let Some(action) = handle_commands(
                client,
                track,
                ui,
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
                start_playback(client, surface, track, target_pts_us, INITIAL_BUFFER_US)?;
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
            start_playback(client, surface, track, target_pts_us, buffered_us.max(1))?;
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

fn handle_commands(
    client: &mut VividClient,
    track: &Track,
    ui: Option<&PlaybackUi>,
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
            Command::Resize(_) => ui.redraw()?,
            Command::Quit => return Ok(Some(ControlAction::Quit)),
        }
    }
    Ok(None)
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
    client: &mut VividClient,
    surface: &vivid_sdk::Surface,
    track: &Track,
    start_pts_us: i64,
    minimum_buffer_us: u64,
) -> io::Result<()> {
    client.wait_track(
        track,
        TrackWaitCondition::MilestoneSet,
        Some(MILESTONE_OUTPUT_READY),
        timeout_us(PLAYBACK_TIMEOUT),
    )?;
    client.activate_tracks(
        surface,
        &[SlotBinding {
            slot: SLOT_AUDIO,
            track_id: track.id(),
            expected_channel_generation: track.channel_generation(),
            required_milestone: MILESTONE_OUTPUT_READY,
        }],
        &RequestMetadata::default(),
    )?;
    client.play(track, start_pts_us, minimum_buffer_us, MAXIMUM_LATENCY_US)?;
    client.wait_track(
        track,
        TrackWaitCondition::PlaybackStarted,
        None,
        timeout_us(PLAYBACK_TIMEOUT),
    )?;
    Ok(())
}

fn audio_surface(context_id: u64, surface_id: u64, path: &Path) -> SurfaceDefinition {
    SurfaceDefinition {
        context_id,
        surface_id,
        semantic_profile: vivid_sdk::GENERIC_CONTENT.into(),
        coordinate_model: CoordinateModel::DesktopLogicalPixels,
        logical_width: 1,
        logical_height: 1,
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
    client: &VividClient,
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
