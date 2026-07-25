use crate::cli::Config;
use crate::ffmpeg::{AudioInfo, VideoInfo};
use crate::protocol::messages;

use vivid_sdk::{AudioConfig, ProducerConfig, VideoConfig};
pub use vivid_sdk::{MediaSender, ProducerSession as VividClient};

impl VideoConfig for VideoInfo {
    fn vivid_video_config(&self, source_id: u64) -> messages::VideoSourceConfig<'_> {
        messages::VideoSourceConfig {
            source_id,
            codec: &self.codec,
            packetization: &self.packetization,
            extradata: &self.extradata,
            width: self.width,
            height: self.height,
            profile: self.profile,
            level: self.level,
            bitrate: self.bitrate,
            color_primaries: self.color_primaries,
            transfer: self.transfer,
            matrix: self.matrix,
            range: self.range,
            sar_num: self.sar_num,
            sar_den: self.sar_den,
            max_access_unit_bytes: self.max_access_unit_bytes,
            codec_string: self.codec_string.as_deref(),
            decoder_config: self.decoder_config.as_deref(),
        }
    }
}

impl AudioConfig for AudioInfo {
    fn vivid_audio_config(
        &self,
        source_id: u64,
        linked_video_source_id: Option<u64>,
    ) -> messages::AudioSourceConfig<'_> {
        messages::AudioSourceConfig {
            source_id,
            linked_video_source_id,
            codec: &self.codec,
            packetization: &self.packetization,
            extradata: &self.extradata,
            sample_rate: self.sample_rate,
            channels: self.channels,
            channel_mask: self.channel_mask,
            bitrate: self.bitrate,
            max_access_unit_bytes: self.max_access_unit_bytes,
            codec_string: self.codec_string.as_deref(),
        }
    }
}

pub fn producer_config(config: &Config) -> ProducerConfig {
    ProducerConfig {
        endpoint: config.endpoint.clone(),
        bulk_endpoint: config.bulk_endpoint.clone(),
        token: config.token.clone(),
        dry_run: config.dry_run,
        trace_dir: config.trace_dir.clone(),
        verbose: config.verbose,
        producer: "vivi".into(),
        producer_version: env!("CARGO_PKG_VERSION").into(),
        required_features: vec![
            messages::FEATURE_RASTER_RGBA8,
            messages::FEATURE_SCENE_TRANSACTIONS,
            messages::FEATURE_GRID_CELL_NODES,
            messages::FEATURE_CREDIT_FLOW_CONTROL,
            messages::FEATURE_TEXT_ANCHORS_V2,
        ],
        optional_features: vec![
            messages::FEATURE_ENCODED_IMAGE_V1,
            messages::FEATURE_RASTER_ZSTD_V1,
            messages::FEATURE_RASTER_PREMULTIPLIED_ALPHA,
            messages::FEATURE_VISIBILITY_EVENTS_V1,
            messages::FEATURE_VIDEO_ACCESS_UNIT_V1,
            messages::FEATURE_VIDEO_CONTROL_V1,
            messages::FEATURE_AUDIO_ACCESS_UNIT_V1,
            messages::FEATURE_DECODER_DESCRIPTION_V1,
        ],
    }
}
