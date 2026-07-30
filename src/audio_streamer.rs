use std::io;
use std::path::Path;
use std::time::Duration;

use vivid_protocol::media::AudioPacket;
use vivid_protocol::messages::LaneClass;
use vivid_protocol::track::{KindConfiguration, TrackConfiguration, TrackMode};
use vivid_sdk::{
    AudioConfiguration, CoordinateModel, MILESTONE_OUTPUT_READY, RequestMetadata, SlotBinding,
    SurfaceDefinition, SurfaceDescriptor, SurfaceRole, Track, TrackWaitCondition,
};

use crate::cli::Config;
use crate::client::VividClient;
use crate::ffmpeg::{AudioDemuxer, AudioInfo};

const INITIAL_BUFFER_US: u64 = 100_000;
const MAXIMUM_LATENCY_US: u64 = 2_000_000;
const PLAYBACK_TIMEOUT: Duration = Duration::from_secs(30);
const SLOT_AUDIO: u64 = 2;

pub fn play(
    config: &Config,
    client: &mut VividClient,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let info = AudioDemuxer::inspect(path)?;
    let mut demuxer = AudioDemuxer::open(path)?;
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
    let channel = client.open_track_channel(&track)?;
    let mut packet_id = 0_u64;
    let mut started = false;
    let mut buffered_us = 0_u64;
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
        buffered_us = buffered_us.saturating_add(packet.duration_us);
        if !started && buffered_us >= INITIAL_BUFFER_US {
            start_playback(
                client,
                &surface,
                &track,
                info.first_pts_us.unwrap_or(0),
                INITIAL_BUFFER_US,
            )?;
            started = true;
        }
    }
    if packet_id == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "audio has no packets").into());
    }
    if !started {
        start_playback(
            client,
            &surface,
            &track,
            info.first_pts_us.unwrap_or(0),
            buffered_us.max(1),
        )?;
    }
    channel.eos()?;
    if !config.no_wait {
        client.drain(&track)?;
        client.wait_track(
            &track,
            TrackWaitCondition::PlaybackEnded,
            None,
            timeout_us(PLAYBACK_TIMEOUT),
        )?;
    }
    client.verbose(format_args!(
        "audio track {} on surface {surface_id}: EOS after {packet_id} packets",
        track.id()
    ));
    client.destroy_track(&track, &RequestMetadata::default())?;
    client.destroy_surface(&surface, &RequestMetadata::default())?;
    Ok(())
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
    Ok(TrackConfiguration {
        context_id: surface.context_id(),
        surface_id: surface.id(),
        track_id: client.allocate_id()?,
        slot: SLOT_AUDIO,
        mode: TrackMode::Timed,
        lane: LaneClass::Realtime,
        maximum_record_body,
        maximum_rate_millihertz: info.maximum_rate_millihertz.max(1),
        maximum_encoded_bits_per_second: info.maximum_encoded_bits_per_second.max(1),
        maximum_records_per_second: info.maximum_records_per_second.max(1),
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
