use std::io;
use std::path::Path;
use std::time::Duration;

use crate::client::VividClient;
use crate::ffmpeg::AudioDemuxer;
use crate::protocol::media::AudioPacket;
use crate::protocol::wire::ConnectionKind;

const INITIAL_BUFFER_US: u64 = 100_000;
const PLAYBACK_START_TIMEOUT: Duration = Duration::from_secs(30);

pub fn play(client: &mut VividClient, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let info = AudioDemuxer::inspect(path)?;
    let mut demuxer = AudioDemuxer::open(path)?;
    let source_id = client.allocate_id()?;
    let source = client.create_audio_source(source_id, None, &info)?;
    let mut sender = client.open_media_sender(source, ConnectionKind::Audio)?;
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
        sender.send_audio(AudioPacket {
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
            if client.supports(crate::protocol::messages::FEATURE_OBSERVABILITY_CORE_V1) {
                client.play_and_wait_until_playing(
                    source_id,
                    info.first_pts_us.unwrap_or(0),
                    INITIAL_BUFFER_US,
                    PLAYBACK_START_TIMEOUT,
                )?;
            } else {
                client.play_at(source_id, info.first_pts_us.unwrap_or(0), INITIAL_BUFFER_US)?;
            }
            started = true;
        }
    }
    if packet_id == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "audio has no packets").into());
    }
    if !started {
        if client.supports(crate::protocol::messages::FEATURE_OBSERVABILITY_CORE_V1) {
            client.play_and_wait_until_playing(
                source_id,
                info.first_pts_us.unwrap_or(0),
                buffered_us,
                PLAYBACK_START_TIMEOUT,
            )?;
        } else {
            client.play_at(source_id, info.first_pts_us.unwrap_or(0), buffered_us)?;
        }
    }
    client.eos_sender(&sender, 1)?;
    client.drain(source_id)?;
    client.verbose(format_args!(
        "audio source {source_id}: EOS after {packet_id} packets"
    ));
    Ok(())
}
