//! Minimal libavformat bindings for Vivid's encoded-packet fast path.
//!
//! Unlike Kitim, Vivi does not decode video into RGBA frames. It demultiplexes the selected video
//! track and forwards encoded access units, timestamps, codec configuration, and keyframe flags.

use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::io;
use std::path::Path;
use std::ptr;

const AVMEDIA_TYPE_VIDEO: c_int = 0;
const AVMEDIA_TYPE_AUDIO: c_int = 1;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
const AV_SAMPLE_FMT_FLT: c_int = 3;
const AV_LOG_QUIET: c_int = -8;
const AV_NOPTS_VALUE: i64 = i64::MIN;
const AV_PKT_DATA_SKIP_SAMPLES: c_int = 11;
const AVSEEK_FLAG_BACKWARD: c_int = 1;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
const AVERROR_EOF: c_int = -541_478_725;
const AVCOL_RANGE_UNSPECIFIED: c_int = 0;
const AVCOL_PRI_UNSPECIFIED: c_int = 2;
const AVCOL_TRC_UNSPECIFIED: c_int = 2;
const AVCOL_SPC_UNSPECIFIED: c_int = 2;
const MAX_EXTRADATA: usize = 16 * 1024 * 1024;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct AVChannelLayout {
    order: c_int,
    nb_channels: c_int,
    mask: u64,
    opaque: *mut c_void,
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
impl Default for AVChannelLayout {
    fn default() -> Self {
        Self {
            order: 0,
            nb_channels: 0,
            mask: 0,
            opaque: ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct AVRational {
    num: c_int,
    den: c_int,
}

#[cfg(not(ffmpeg_old_channel_layout))]
#[repr(C)]
struct AVCodecParameters {
    codec_type: c_int,
    codec_id: u32,
    codec_tag: u32,
    _pad1: u32,
    extradata: *mut u8,
    extradata_size: c_int,
    _pad2: u32,
    coded_side_data: *mut c_void,
    nb_coded_side_data: c_int,
    format: c_int,
    bit_rate: i64,
    bits_per_coded_sample: c_int,
    bits_per_raw_sample: c_int,
    profile: c_int,
    level: c_int,
    width: c_int,
    height: c_int,
    sample_aspect_ratio: AVRational,
    #[cfg(ffmpeg_codecpar_has_framerate)]
    framerate: AVRational,
    field_order: c_int,
    color_range: c_int,
    color_primaries: c_int,
    color_trc: c_int,
    color_space: c_int,
    chroma_location: c_int,
    video_delay: c_int,
    ch_layout: AVChannelLayout,
    sample_rate: c_int,
}

#[cfg(ffmpeg_old_channel_layout)]
#[repr(C)]
struct AVCodecParameters {
    codec_type: c_int,
    codec_id: u32,
    codec_tag: u32,
    extradata: *mut u8,
    extradata_size: c_int,
    format: c_int,
    bit_rate: i64,
    bits_per_coded_sample: c_int,
    bits_per_raw_sample: c_int,
    profile: c_int,
    level: c_int,
    width: c_int,
    height: c_int,
    sample_aspect_ratio: AVRational,
    field_order: c_int,
    color_range: c_int,
    color_primaries: c_int,
    color_trc: c_int,
    color_space: c_int,
    chroma_location: c_int,
    video_delay: c_int,
    channel_layout: u64,
    channels: c_int,
    sample_rate: c_int,
    block_align: c_int,
    frame_size: c_int,
    initial_padding: c_int,
    trailing_padding: c_int,
    seek_preroll: c_int,
    ch_layout: AVChannelLayout,
    framerate: AVRational,
    coded_side_data: *mut c_void,
    nb_coded_side_data: c_int,
}

#[repr(C)]
struct AVStream {
    av_class: *const c_void,
    index: c_int,
    id: c_int,
    codecpar: *mut AVCodecParameters,
    priv_data: *mut c_void,
    time_base: AVRational,
    start_time: i64,
    duration: i64,
    nb_frames: i64,
    disposition: c_int,
    discard: c_int,
    sample_aspect_ratio: AVRational,
    metadata: *mut c_void,
    avg_frame_rate: AVRational,
}

#[repr(C)]
struct AVFormatContext {
    av_class: *const c_void,
    iformat: *mut c_void,
    oformat: *mut c_void,
    priv_data: *mut c_void,
    pb: *mut c_void,
    ctx_flags: c_int,
    nb_streams: c_uint,
    streams: *mut *mut AVStream,
}

#[repr(C)]
struct AVPacket {
    buf: *mut c_void,
    pts: i64,
    dts: i64,
    data: *mut u8,
    size: c_int,
    stream_index: c_int,
    flags: c_int,
    side_data: *mut c_void,
    side_data_elems: c_int,
    duration: i64,
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[repr(C)]
struct AVFrame {
    data: [*mut u8; 8],
    linesize: [c_int; 8],
    extended_data: *mut *mut u8,
    width: c_int,
    height: c_int,
    nb_samples: c_int,
    format: c_int,
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
enum AVCodecContext {}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
enum SwrContext {}

unsafe extern "C" {
    fn av_log_set_level(level: c_int);
    fn avformat_open_input(
        context: *mut *mut AVFormatContext,
        url: *const c_char,
        format: *mut c_void,
        options: *mut *mut c_void,
    ) -> c_int;
    fn avformat_find_stream_info(context: *mut AVFormatContext, options: *mut *mut c_void)
    -> c_int;
    fn avformat_close_input(context: *mut *mut AVFormatContext);
    fn av_read_frame(context: *mut AVFormatContext, packet: *mut AVPacket) -> c_int;
    fn av_seek_frame(
        context: *mut AVFormatContext,
        stream_index: c_int,
        timestamp: i64,
        flags: c_int,
    ) -> c_int;
    fn avformat_flush(context: *mut AVFormatContext);
    fn av_packet_alloc() -> *mut AVPacket;
    fn av_packet_unref(packet: *mut AVPacket);
    fn av_packet_get_side_data(
        packet: *const AVPacket,
        side_data_type: c_int,
        size: *mut usize,
    ) -> *mut u8;
    fn av_packet_free(packet: *mut *mut AVPacket);
    fn avcodec_get_name(codec_id: u32) -> *const c_char;
    fn av_strerror(error: c_int, buffer: *mut c_char, buffer_size: usize) -> c_int;

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn avcodec_find_decoder(codec_id: u32) -> *const c_void;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn avcodec_alloc_context3(codec: *const c_void) -> *mut AVCodecContext;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn avcodec_parameters_to_context(
        codec: *mut AVCodecContext,
        parameters: *const AVCodecParameters,
    ) -> c_int;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn avcodec_open2(
        codec: *mut AVCodecContext,
        decoder: *const c_void,
        options: *mut *mut c_void,
    ) -> c_int;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn avcodec_free_context(codec: *mut *mut AVCodecContext);
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn avcodec_send_packet(codec: *mut AVCodecContext, packet: *const AVPacket) -> c_int;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn avcodec_receive_frame(codec: *mut AVCodecContext, frame: *mut AVFrame) -> c_int;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn av_frame_alloc() -> *mut AVFrame;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn av_frame_free(frame: *mut *mut AVFrame);
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn av_channel_layout_default(layout: *mut AVChannelLayout, channels: c_int);
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn av_channel_layout_copy(
        destination: *mut AVChannelLayout,
        source: *const AVChannelLayout,
    ) -> c_int;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn av_channel_layout_uninit(layout: *mut AVChannelLayout);
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn swr_alloc_set_opts2(
        context: *mut *mut SwrContext,
        output_layout: *const AVChannelLayout,
        output_format: c_int,
        output_rate: c_int,
        input_layout: *const AVChannelLayout,
        input_format: c_int,
        input_rate: c_int,
        log_offset: c_int,
        log_context: *mut c_void,
    ) -> c_int;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn swr_init(context: *mut SwrContext) -> c_int;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn swr_convert(
        context: *mut SwrContext,
        output: *mut *mut u8,
        output_count: c_int,
        input: *const *const u8,
        input_count: c_int,
    ) -> c_int;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn swr_free(context: *mut *mut SwrContext);
}

#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub codec: String,
    pub packetization: String,
    pub extradata: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub profile: i32,
    pub level: i32,
    pub bitrate: i64,
    pub color_primaries: u64,
    pub transfer: u64,
    pub matrix: u64,
    pub range: u64,
    pub colorimetry_inferred: bool,
    pub sar_num: u32,
    pub sar_den: u32,
    pub max_access_unit_bytes: u32,
    pub maximum_rate_millihertz: u64,
    pub maximum_encoded_bits_per_second: u64,
    pub maximum_records_per_second: u64,
    pub first_pts_us: Option<i64>,
    pub duration_us: Option<u64>,
    pub has_audio: bool,
    pub audio: Option<AudioInfo>,
    /// RFC 6381 codec string derived from the container decoder configuration
    /// (`decoder-description-v1`).
    pub codec_string: Option<String>,
    /// Original ISO-BMFF decoder configuration box body (avcC/hvcC/av1C) before Annex-B
    /// normalization (`decoder-description-v1`).
    pub decoder_config: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct AudioInfo {
    pub codec: String,
    pub packetization: String,
    pub extradata: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u16,
    pub channel_mask: u64,
    pub bitrate: i64,
    pub max_access_unit_bytes: u32,
    pub maximum_rate_millihertz: u64,
    pub maximum_encoded_bits_per_second: u64,
    pub maximum_records_per_second: u64,
    pub first_pts_us: Option<i64>,
    pub duration_us: Option<u64>,
    /// RFC 6381 codec string (`decoder-description-v1`).
    pub codec_string: Option<String>,
}

#[derive(Debug, Default)]
struct RateClaims {
    window: VecDeque<(i64, u64)>,
    window_bytes: u64,
    maximum_records: u64,
    maximum_bytes: u64,
    untimed_records: u64,
    untimed_bytes: u64,
}

fn packet_timestamp(dts_us: i64, pts_us: i64) -> i64 {
    if dts_us != AV_NOPTS_VALUE {
        dts_us
    } else {
        pts_us
    }
}

impl RateClaims {
    fn observe(&mut self, timestamp_us: i64, encoded_bytes: usize) -> io::Result<()> {
        if timestamp_us == AV_NOPTS_VALUE {
            self.untimed_records = self.untimed_records.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "packet count overflow")
            })?;
            self.untimed_bytes = self
                .untimed_bytes
                .checked_add(u64::try_from(encoded_bytes).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "packet length exceeds u64")
                })?)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "packet bytes overflow")
                })?;
            return Ok(());
        }
        let encoded_bytes = u64::try_from(encoded_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "packet length exceeds u64"))?;
        if self
            .window
            .back()
            .is_some_and(|(previous, _)| timestamp_us < *previous)
        {
            self.window.clear();
            self.window_bytes = 0;
        }
        let cutoff = timestamp_us.saturating_sub(1_000_000);
        while self
            .window
            .front()
            .is_some_and(|(timestamp, _)| *timestamp <= cutoff)
        {
            let (_, bytes) = self.window.pop_front().expect("front entry exists");
            self.window_bytes = self.window_bytes.checked_sub(bytes).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "packet rate accounting underflow",
                )
            })?;
        }
        self.window.push_back((timestamp_us, encoded_bytes));
        self.window_bytes = self
            .window_bytes
            .checked_add(encoded_bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "packet bytes overflow"))?;
        self.maximum_records = self
            .maximum_records
            .max(u64::try_from(self.window.len()).unwrap_or(u64::MAX));
        self.maximum_bytes = self.maximum_bytes.max(self.window_bytes);
        Ok(())
    }

    fn apply_video(&self, info: &mut VideoInfo) {
        let maximum_record_body = u64::from(
            vivid_protocol::media::video_body_len(info.max_access_unit_bytes)
                .unwrap_or(info.max_access_unit_bytes),
        );
        let records = self.effective_records();
        info.maximum_records_per_second = records;
        info.maximum_rate_millihertz = records.saturating_mul(1_000);
        info.maximum_encoded_bits_per_second = self
            .effective_bits(maximum_record_body)
            .max(u64::try_from(info.bitrate.max(0)).unwrap_or(0))
            .max(1);
    }

    fn apply_audio(&self, info: &mut AudioInfo) {
        let maximum_record_body = u64::from(
            vivid_protocol::media::audio_body_len(info.max_access_unit_bytes)
                .unwrap_or(info.max_access_unit_bytes),
        );
        let records = self.effective_records();
        info.maximum_records_per_second = records;
        info.maximum_rate_millihertz = records.saturating_mul(1_000);
        info.maximum_encoded_bits_per_second = self
            .effective_bits(maximum_record_body)
            .max(u64::try_from(info.bitrate.max(0)).unwrap_or(0))
            .max(1);
    }

    fn effective_records(&self) -> u64 {
        self.maximum_records
            .saturating_add(self.untimed_records)
            .max(1)
    }

    fn effective_bits(&self, maximum_record_body: u64) -> u64 {
        let observed = self.maximum_bytes.saturating_add(self.untimed_bytes);
        let bytes = observed.max(maximum_record_body);
        bytes.saturating_mul(8)
    }
}

#[derive(Debug)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub pts_us: i64,
    pub dts_us: i64,
    pub duration_us: u64,
    pub key: bool,
}

#[derive(Debug)]
pub struct EncodedAudioPacket {
    pub data: Vec<u8>,
    pub pts_us: i64,
    pub dts_us: i64,
    pub duration_us: u64,
    pub trim_start_samples: u32,
    pub trim_end_samples: u32,
}

#[derive(Debug)]
pub enum EncodedMediaPacket {
    Video(EncodedPacket),
    Audio(EncodedAudioPacket),
}

pub struct VideoDemuxer {
    context: *mut AVFormatContext,
    packet: *mut AVPacket,
    stream_index: c_int,
    time_base: AVRational,
    info: VideoInfo,
    nal_length_size: Option<usize>,
    audio_stream_index: Option<c_int>,
    audio_time_base: Option<AVRational>,
}

impl VideoDemuxer {
    pub fn open(path: &Path) -> io::Result<Self> {
        let path = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "media path contains a NUL byte",
            )
        })?;

        unsafe { av_log_set_level(AV_LOG_QUIET) };
        let mut context = ptr::null_mut();
        let result = unsafe {
            avformat_open_input(
                &mut context,
                path.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if result < 0 {
            return Err(ffmpeg_error("could not open media", result));
        }

        let stream_result = unsafe { avformat_find_stream_info(context, ptr::null_mut()) };
        if stream_result < 0 {
            unsafe { avformat_close_input(&mut context) };
            return Err(ffmpeg_error(
                "could not inspect media streams",
                stream_result,
            ));
        }

        let selected = unsafe { find_video_stream(context) };
        let (stream_index, stream, parameters) = match selected {
            Some(selected) => selected,
            None => {
                unsafe { avformat_close_input(&mut context) };
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "media has no video stream",
                ));
            }
        };

        let packet = unsafe { av_packet_alloc() };
        if packet.is_null() {
            unsafe { avformat_close_input(&mut context) };
            return Err(io::Error::other("FFmpeg could not allocate a packet"));
        }

        let (mut info, nal_length_size) = unsafe { video_info(parameters)? };
        let selected_audio = unsafe { find_audio_stream(context) };
        info.has_audio = selected_audio.is_some();
        let (audio_stream_index, audio_time_base, audio) = match selected_audio {
            Some((index, stream, parameters)) => match unsafe { audio_info(parameters, stream) } {
                Ok(info) => (
                    Some(index),
                    Some(unsafe { (*stream).time_base }),
                    Some(info),
                ),
                Err(_) => (None, None, None),
            },
            None => (None, None, None),
        };
        info.audio = audio;
        let time_base = unsafe { (*stream).time_base };
        if time_base.num <= 0 || time_base.den <= 0 {
            let mut packet = packet;
            unsafe {
                av_packet_free(&mut packet);
                avformat_close_input(&mut context);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid video time base",
            ));
        }

        Ok(Self {
            context,
            packet,
            stream_index,
            time_base,
            info,
            nal_length_size,
            audio_stream_index,
            audio_time_base,
        })
    }

    pub fn inspect(path: &Path) -> io::Result<VideoInfo> {
        let mut demuxer = Self::open(path)?;
        let mut maximum = 0_usize;
        let mut audio_maximum = 0_usize;
        let mut video_rate = RateClaims::default();
        let mut audio_rate = RateClaims::default();
        let mut video_end_pts = None;
        let mut audio_end_pts = None;
        while let Some(packet) = demuxer.next_media_packet()? {
            match packet {
                EncodedMediaPacket::Video(packet) => {
                    maximum = maximum.max(packet.data.len());
                    let body_length = vivid_protocol::media::video_body_len(
                        u32::try_from(packet.data.len()).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "video access unit exceeds u32",
                            )
                        })?,
                    )
                    .map_err(io::Error::other)?;
                    video_rate.observe(
                        packet_timestamp(packet.dts_us, packet.pts_us),
                        usize::try_from(body_length).unwrap_or(usize::MAX),
                    )?;
                    if packet.pts_us != AV_NOPTS_VALUE {
                        demuxer.info.first_pts_us.get_or_insert(packet.pts_us);
                        video_end_pts = Some(video_end_pts.map_or(
                            packet_end_pts(packet.pts_us, packet.duration_us),
                            |end: i64| end.max(packet_end_pts(packet.pts_us, packet.duration_us)),
                        ));
                    }
                }
                EncodedMediaPacket::Audio(packet) => {
                    audio_maximum = audio_maximum.max(packet.data.len());
                    let body_length = vivid_protocol::media::audio_body_len(
                        u32::try_from(packet.data.len()).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "audio access unit exceeds u32",
                            )
                        })?,
                    )
                    .map_err(io::Error::other)?;
                    audio_rate.observe(
                        packet_timestamp(packet.dts_us, packet.pts_us),
                        usize::try_from(body_length).unwrap_or(usize::MAX),
                    )?;
                    if packet.pts_us != AV_NOPTS_VALUE
                        && let Some(info) = demuxer.info.audio.as_mut()
                    {
                        info.first_pts_us.get_or_insert(packet.pts_us);
                        audio_end_pts = Some(audio_end_pts.map_or(
                            packet_end_pts(packet.pts_us, packet.duration_us),
                            |end: i64| end.max(packet_end_pts(packet.pts_us, packet.duration_us)),
                        ));
                    }
                }
            }
        }
        if maximum == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "video has no access units",
            ));
        }
        demuxer.info.max_access_unit_bytes = u32::try_from(maximum)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "access unit exceeds u32"))?;
        video_rate.apply_video(&mut demuxer.info);
        demuxer.info.duration_us = duration_between(demuxer.info.first_pts_us, video_end_pts);
        if let Some(audio) = demuxer.info.audio.as_mut() {
            audio.max_access_unit_bytes = u32::try_from(audio_maximum).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "audio access unit exceeds u32")
            })?;
            if audio.max_access_unit_bytes == 0 {
                demuxer.info.audio = None;
            } else {
                audio_rate.apply_audio(audio);
                audio.duration_us = duration_between(audio.first_pts_us, audio_end_pts);
            }
        }
        Ok(demuxer.info.clone())
    }

    pub fn next_media_packet(&mut self) -> io::Result<Option<EncodedMediaPacket>> {
        loop {
            unsafe { av_packet_unref(self.packet) };
            let result = unsafe { av_read_frame(self.context, self.packet) };
            if result < 0 {
                return Ok(None);
            }

            let packet = unsafe { &*self.packet };
            if packet.stream_index != self.stream_index
                && Some(packet.stream_index) != self.audio_stream_index
            {
                continue;
            }
            if packet.size < 0 || (packet.size > 0 && packet.data.is_null()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid FFmpeg packet",
                ));
            }

            let mut data = if packet.size == 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(packet.data, packet.size as usize) }.to_vec()
            };
            if packet.stream_index == self.stream_index {
                if let Some(length_size) = self.nal_length_size {
                    data = length_prefixed_to_annex_b(&data, length_size)?;
                }
                return Ok(Some(EncodedMediaPacket::Video(EncodedPacket {
                    key: vivid_protocol::media::access_unit_is_key(&self.info.codec, &data)?,
                    data,
                    pts_us: timestamp_us(packet.pts, self.time_base),
                    dts_us: timestamp_us(packet.dts, self.time_base),
                    duration_us: timestamp_duration_us(packet.duration, self.time_base),
                })));
            }
            let time_base = self.audio_time_base.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "audio stream has no time base")
            })?;
            let (mut trim_start_samples, trim_end_samples) = packet_trim(packet);
            if self
                .info
                .audio
                .as_ref()
                .is_some_and(|audio| audio.codec == "opus")
            {
                // OpusHead pre-skip is applied by the presenter decoder. FFmpeg also exposes the
                // same initial discard as packet side data; carrying both would trim twice.
                trim_start_samples = 0;
            }
            return Ok(Some(EncodedMediaPacket::Audio(EncodedAudioPacket {
                data,
                pts_us: timestamp_us(packet.pts, time_base),
                dts_us: timestamp_us(packet.dts, time_base),
                duration_us: timestamp_duration_us(packet.duration, time_base),
                trim_start_samples,
                trim_end_samples,
            })));
        }
    }

    pub fn seek_to_us(&mut self, target_pts_us: i64) -> io::Result<()> {
        seek_context(
            self.context,
            self.packet,
            self.stream_index,
            self.time_base,
            target_pts_us,
        )
    }
}

impl Drop for VideoDemuxer {
    fn drop(&mut self) {
        unsafe {
            av_packet_free(&mut self.packet);
            avformat_close_input(&mut self.context);
        }
    }
}

pub struct AudioDemuxer {
    context: *mut AVFormatContext,
    packet: *mut AVPacket,
    stream_index: c_int,
    time_base: AVRational,
    info: AudioInfo,
}

impl AudioDemuxer {
    pub fn open(path: &Path) -> io::Result<Self> {
        let path = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "media path contains a NUL byte",
            )
        })?;
        unsafe { av_log_set_level(AV_LOG_QUIET) };
        let mut context = ptr::null_mut();
        let result = unsafe {
            avformat_open_input(
                &mut context,
                path.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if result < 0 {
            return Err(ffmpeg_error("could not open audio media", result));
        }
        let result = unsafe { avformat_find_stream_info(context, ptr::null_mut()) };
        if result < 0 {
            unsafe { avformat_close_input(&mut context) };
            return Err(ffmpeg_error("could not inspect audio streams", result));
        }
        let Some((stream_index, stream, parameters)) = (unsafe { find_audio_stream(context) })
        else {
            unsafe { avformat_close_input(&mut context) };
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "media has no audio stream",
            ));
        };
        let time_base = unsafe { (*stream).time_base };
        if time_base.num <= 0 || time_base.den <= 0 {
            unsafe { avformat_close_input(&mut context) };
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid audio time base",
            ));
        }
        let info = unsafe { audio_info(parameters, stream)? };
        let packet = unsafe { av_packet_alloc() };
        if packet.is_null() {
            unsafe { avformat_close_input(&mut context) };
            return Err(io::Error::other(
                "FFmpeg could not allocate an audio packet",
            ));
        }
        Ok(Self {
            context,
            packet,
            stream_index,
            time_base,
            info,
        })
    }

    pub fn inspect(path: &Path) -> io::Result<AudioInfo> {
        let mut demuxer = Self::open(path)?;
        let mut maximum = 0_usize;
        let mut rate = RateClaims::default();
        let mut end_pts = None;
        while let Some(packet) = demuxer.next_packet()? {
            maximum = maximum.max(packet.data.len());
            let body_length =
                vivid_protocol::media::audio_body_len(u32::try_from(packet.data.len()).map_err(
                    |_| io::Error::new(io::ErrorKind::InvalidData, "audio access unit exceeds u32"),
                )?)
                .map_err(io::Error::other)?;
            rate.observe(
                packet_timestamp(packet.dts_us, packet.pts_us),
                usize::try_from(body_length).unwrap_or(usize::MAX),
            )?;
            if packet.pts_us != AV_NOPTS_VALUE {
                demuxer.info.first_pts_us.get_or_insert(packet.pts_us);
                end_pts = Some(end_pts.map_or(
                    packet_end_pts(packet.pts_us, packet.duration_us),
                    |end: i64| end.max(packet_end_pts(packet.pts_us, packet.duration_us)),
                ));
            }
        }
        if maximum == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "audio access unit is empty",
            ));
        }
        demuxer.info.max_access_unit_bytes = u32::try_from(maximum).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "audio access unit exceeds u32")
        })?;
        rate.apply_audio(&mut demuxer.info);
        demuxer.info.duration_us = duration_between(demuxer.info.first_pts_us, end_pts);
        Ok(demuxer.info.clone())
    }

    pub fn seek_to_us(&mut self, target_pts_us: i64) -> io::Result<()> {
        seek_context(
            self.context,
            self.packet,
            self.stream_index,
            self.time_base,
            target_pts_us,
        )
    }

    pub fn next_packet(&mut self) -> io::Result<Option<EncodedAudioPacket>> {
        loop {
            unsafe { av_packet_unref(self.packet) };
            let result = unsafe { av_read_frame(self.context, self.packet) };
            if result < 0 {
                return Ok(None);
            }
            let packet = unsafe { &*self.packet };
            if packet.stream_index != self.stream_index {
                continue;
            }
            if packet.size < 0 || (packet.size > 0 && packet.data.is_null()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid audio packet",
                ));
            }
            let data = if packet.size == 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(packet.data, packet.size as usize) }.to_vec()
            };
            let (mut trim_start_samples, trim_end_samples) = packet_trim(packet);
            if self.info.codec == "opus" {
                trim_start_samples = 0;
            }
            return Ok(Some(EncodedAudioPacket {
                data,
                pts_us: timestamp_us(packet.pts, self.time_base),
                dts_us: timestamp_us(packet.dts, self.time_base),
                duration_us: timestamp_duration_us(packet.duration, self.time_base),
                trim_start_samples,
                trim_end_samples,
            }));
        }
    }
}

impl Drop for AudioDemuxer {
    fn drop(&mut self) {
        unsafe {
            av_packet_free(&mut self.packet);
            avformat_close_input(&mut self.context);
        }
    }
}

unsafe fn find_video_stream(
    context: *mut AVFormatContext,
) -> Option<(c_int, *mut AVStream, *mut AVCodecParameters)> {
    let context = unsafe { &*context };
    for index in 0..context.nb_streams as usize {
        let stream = unsafe { *context.streams.add(index) };
        if stream.is_null() {
            continue;
        }
        let parameters = unsafe { (*stream).codecpar };
        if !parameters.is_null() && unsafe { (*parameters).codec_type } == AVMEDIA_TYPE_VIDEO {
            return Some((index as c_int, stream, parameters));
        }
    }
    None
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[derive(Debug)]
pub struct DecodedAudio {
    pub samples: Vec<f32>,
}

/// FFmpeg audio decoder and software resampler used by the local output worker.
///
/// The value is intentionally created and consumed on one worker thread. Raw FFmpeg pointers never
/// cross the thread boundary.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub struct AudioDecoder {
    context: *mut AVFormatContext,
    codec: *mut AVCodecContext,
    packet: *mut AVPacket,
    frame: *mut AVFrame,
    resampler: *mut SwrContext,
    parameters: *mut AVCodecParameters,
    stream_index: c_int,
    time_base: AVRational,
    input_sample_rate: c_int,
    output_sample_rate: c_int,
    output_channels: c_int,
    first_pts_us: Option<i64>,
    input_eof: bool,
    resampler_drained: bool,
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
impl AudioDecoder {
    pub fn open(path: &Path, output_sample_rate: u32, output_channels: u16) -> io::Result<Self> {
        let path = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "media path contains a NUL byte",
            )
        })?;
        let output_sample_rate = c_int::try_from(output_sample_rate).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "output sample rate is too large",
            )
        })?;
        let output_channels = c_int::from(output_channels);
        if output_sample_rate <= 0 || output_channels <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid audio output configuration",
            ));
        }

        unsafe { av_log_set_level(AV_LOG_QUIET) };
        let mut context = ptr::null_mut();
        let open_result = unsafe {
            avformat_open_input(
                &mut context,
                path.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if open_result < 0 {
            return Err(ffmpeg_error("could not open audio media", open_result));
        }

        let stream_result = unsafe { avformat_find_stream_info(context, ptr::null_mut()) };
        if stream_result < 0 {
            unsafe { avformat_close_input(&mut context) };
            return Err(ffmpeg_error(
                "could not inspect audio streams",
                stream_result,
            ));
        }

        let Some((stream_index, stream, parameters)) = (unsafe { find_audio_stream(context) })
        else {
            unsafe { avformat_close_input(&mut context) };
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "media has no audio stream",
            ));
        };
        let input_sample_rate = unsafe { (*parameters).sample_rate };
        if input_sample_rate <= 0 {
            unsafe { avformat_close_input(&mut context) };
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "audio stream has no sample rate",
            ));
        }
        let time_base = unsafe { (*stream).time_base };
        let stream_start = unsafe { (*stream).start_time };
        let first_pts_us =
            (stream_start != AV_NOPTS_VALUE && time_base.num > 0 && time_base.den > 0)
                .then(|| timestamp_us(stream_start, time_base));

        let decoder = unsafe { avcodec_find_decoder((*parameters).codec_id) };
        if decoder.is_null() {
            unsafe { avformat_close_input(&mut context) };
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "FFmpeg has no decoder for the audio stream",
            ));
        }
        let mut codec = unsafe { avcodec_alloc_context3(decoder) };
        if codec.is_null() {
            unsafe { avformat_close_input(&mut context) };
            return Err(io::Error::other(
                "FFmpeg could not allocate an audio decoder context",
            ));
        }
        let parameters_result = unsafe { avcodec_parameters_to_context(codec, parameters) };
        if parameters_result < 0 {
            unsafe {
                avcodec_free_context(&mut codec);
                avformat_close_input(&mut context);
            }
            return Err(ffmpeg_error(
                "could not configure the audio decoder",
                parameters_result,
            ));
        }
        let decoder_result = unsafe { avcodec_open2(codec, decoder, ptr::null_mut()) };
        if decoder_result < 0 {
            unsafe {
                avcodec_free_context(&mut codec);
                avformat_close_input(&mut context);
            }
            return Err(ffmpeg_error(
                "could not open the audio decoder",
                decoder_result,
            ));
        }

        let mut packet = unsafe { av_packet_alloc() };
        if packet.is_null() {
            unsafe {
                avcodec_free_context(&mut codec);
                avformat_close_input(&mut context);
            }
            return Err(io::Error::other(
                "FFmpeg could not allocate an audio packet",
            ));
        }
        let frame = unsafe { av_frame_alloc() };
        if frame.is_null() {
            unsafe {
                av_packet_free(&mut packet);
                avcodec_free_context(&mut codec);
                avformat_close_input(&mut context);
            }
            return Err(io::Error::other("FFmpeg could not allocate an audio frame"));
        }

        Ok(Self {
            context,
            codec,
            packet,
            frame,
            resampler: ptr::null_mut(),
            parameters,
            stream_index,
            time_base,
            input_sample_rate,
            output_sample_rate,
            output_channels,
            first_pts_us,
            input_eof: false,
            resampler_drained: false,
        })
    }

    pub fn first_pts_us(&self) -> Option<i64> {
        self.first_pts_us
    }

    pub fn next_frame(&mut self) -> io::Result<Option<DecodedAudio>> {
        loop {
            let receive_result = unsafe { avcodec_receive_frame(self.codec, self.frame) };
            if receive_result == 0 {
                if let Some(frame) = self.convert_frame()? {
                    return Ok(Some(frame));
                }
                continue;
            }
            if receive_result != -libc::EAGAIN && receive_result != AVERROR_EOF {
                return Err(ffmpeg_error(
                    "could not decode an audio frame",
                    receive_result,
                ));
            }

            if self.input_eof {
                return self.flush_resampler();
            }

            let mut found_audio = false;
            loop {
                unsafe { av_packet_unref(self.packet) };
                let read_result = unsafe { av_read_frame(self.context, self.packet) };
                if read_result < 0 {
                    let flush_result = unsafe { avcodec_send_packet(self.codec, ptr::null()) };
                    if flush_result < 0 && flush_result != AVERROR_EOF {
                        return Err(ffmpeg_error(
                            "could not flush the audio decoder",
                            flush_result,
                        ));
                    }
                    self.input_eof = true;
                    break;
                }
                let packet = unsafe { &*self.packet };
                if packet.stream_index != self.stream_index {
                    continue;
                }
                if self.first_pts_us.is_none() && packet.pts != AV_NOPTS_VALUE {
                    self.first_pts_us = Some(timestamp_us(packet.pts, self.time_base));
                }
                found_audio = true;
                break;
            }

            if found_audio {
                let send_result = unsafe { avcodec_send_packet(self.codec, self.packet) };
                unsafe { av_packet_unref(self.packet) };
                if send_result < 0 && send_result != -libc::EAGAIN {
                    return Err(ffmpeg_error(
                        "could not submit an audio packet",
                        send_result,
                    ));
                }
            }
        }
    }

    fn convert_frame(&mut self) -> io::Result<Option<DecodedAudio>> {
        let frame = unsafe { &*self.frame };
        if frame.nb_samples <= 0 {
            return Ok(None);
        }
        if self.resampler.is_null() {
            self.initialize_resampler(frame.format)?;
        }

        let maximum_samples = (i64::from(frame.nb_samples)
            .saturating_mul(i64::from(self.output_sample_rate))
            / i64::from(self.input_sample_rate)
            + 256)
            .clamp(1, i64::from(c_int::MAX)) as c_int;
        let sample_count = usize::try_from(maximum_samples)
            .ok()
            .and_then(|samples| samples.checked_mul(self.output_channels as usize))
            .ok_or_else(|| io::Error::other("resampled audio frame is too large"))?;
        let mut samples = vec![0.0_f32; sample_count];
        let mut output = [ptr::null_mut(); 8];
        output[0] = samples.as_mut_ptr().cast();
        let input = if frame.extended_data.is_null() {
            frame.data.as_ptr() as *const *const u8
        } else {
            frame.extended_data as *const *const u8
        };
        let converted = unsafe {
            swr_convert(
                self.resampler,
                output.as_mut_ptr(),
                maximum_samples,
                input,
                frame.nb_samples,
            )
        };
        if converted < 0 {
            return Err(ffmpeg_error("could not resample audio", converted));
        }
        samples.truncate(converted as usize * self.output_channels as usize);
        Ok((!samples.is_empty()).then_some(DecodedAudio { samples }))
    }

    fn initialize_resampler(&mut self, input_format: c_int) -> io::Result<()> {
        let mut input_layout = AVChannelLayout::default();
        let parameters = unsafe { &*self.parameters };
        if parameters.ch_layout.nb_channels > 0 {
            let copy_result =
                unsafe { av_channel_layout_copy(&mut input_layout, &parameters.ch_layout) };
            if copy_result < 0 {
                return Err(ffmpeg_error(
                    "could not copy the input channel layout",
                    copy_result,
                ));
            }
        } else {
            unsafe { av_channel_layout_default(&mut input_layout, 2) };
        }

        let mut output_layout = AVChannelLayout::default();
        unsafe { av_channel_layout_default(&mut output_layout, self.output_channels) };
        let result = unsafe {
            swr_alloc_set_opts2(
                &mut self.resampler,
                &output_layout,
                AV_SAMPLE_FMT_FLT,
                self.output_sample_rate,
                &input_layout,
                input_format,
                self.input_sample_rate,
                0,
                ptr::null_mut(),
            )
        };
        unsafe {
            av_channel_layout_uninit(&mut input_layout);
            av_channel_layout_uninit(&mut output_layout);
        }
        if result < 0 || self.resampler.is_null() {
            return Err(ffmpeg_error(
                "could not allocate the audio resampler",
                result,
            ));
        }
        let init_result = unsafe { swr_init(self.resampler) };
        if init_result < 0 {
            unsafe { swr_free(&mut self.resampler) };
            return Err(ffmpeg_error(
                "could not initialize the audio resampler",
                init_result,
            ));
        }
        Ok(())
    }

    fn flush_resampler(&mut self) -> io::Result<Option<DecodedAudio>> {
        if self.resampler.is_null() || self.resampler_drained {
            return Ok(None);
        }
        let maximum_samples = 4096 as c_int;
        let mut samples = vec![0.0_f32; maximum_samples as usize * self.output_channels as usize];
        let mut output = [ptr::null_mut(); 8];
        output[0] = samples.as_mut_ptr().cast();
        let converted = unsafe {
            swr_convert(
                self.resampler,
                output.as_mut_ptr(),
                maximum_samples,
                ptr::null(),
                0,
            )
        };
        if converted < 0 {
            return Err(ffmpeg_error("could not drain resampled audio", converted));
        }
        if converted == 0 {
            self.resampler_drained = true;
            return Ok(None);
        }
        samples.truncate(converted as usize * self.output_channels as usize);
        Ok(Some(DecodedAudio { samples }))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
impl Drop for AudioDecoder {
    fn drop(&mut self) {
        unsafe {
            swr_free(&mut self.resampler);
            av_frame_free(&mut self.frame);
            av_packet_free(&mut self.packet);
            avcodec_free_context(&mut self.codec);
            avformat_close_input(&mut self.context);
        }
    }
}

unsafe fn find_audio_stream(
    context: *mut AVFormatContext,
) -> Option<(c_int, *mut AVStream, *mut AVCodecParameters)> {
    let context = unsafe { &*context };
    for index in 0..context.nb_streams as usize {
        let stream = unsafe { *context.streams.add(index) };
        if stream.is_null() {
            continue;
        }
        let parameters = unsafe { (*stream).codecpar };
        if !parameters.is_null() && unsafe { (*parameters).codec_type } == AVMEDIA_TYPE_AUDIO {
            return Some((index as c_int, stream, parameters));
        }
    }
    None
}

unsafe fn video_info(parameters: *mut AVCodecParameters) -> io::Result<(VideoInfo, Option<usize>)> {
    let parameters = unsafe { &*parameters };
    let width = u32::try_from(parameters.width)
        .ok()
        .filter(|width| *width > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid video width"))?;
    let height = u32::try_from(parameters.height)
        .ok()
        .filter(|height| *height > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid video height"))?;

    let codec_pointer = unsafe { avcodec_get_name(parameters.codec_id) };
    let codec = if codec_pointer.is_null() {
        format!("ffmpeg-codec-{}", parameters.codec_id)
    } else {
        unsafe { CStr::from_ptr(codec_pointer) }
            .to_string_lossy()
            .into_owned()
    };

    let extradata_size = usize::try_from(parameters.extradata_size)
        .ok()
        .filter(|size| *size <= MAX_EXTRADATA)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid codec extradata size")
        })?;
    let extradata = if extradata_size == 0 {
        Vec::new()
    } else if parameters.extradata.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "codec extradata pointer is null",
        ));
    } else {
        unsafe { std::slice::from_raw_parts(parameters.extradata, extradata_size) }.to_vec()
    };

    // Retain the container decoder-configuration box (avcC/hvcC/av1C) before wire
    // normalization so `decoder-description-v1` can describe the original stream.
    let container_box = (!extradata.is_empty() && extradata[0] != 0).then(|| extradata.clone());
    let (packetization, extradata, nal_length_size) = match codec.as_str() {
        "h264" => {
            let (data, length) = normalize_h26x_extradata(&extradata, false)?;
            ("h264-annexb-au-v1".to_owned(), data, length)
        }
        "hevc" => {
            let (data, length) = normalize_h26x_extradata(&extradata, true)?;
            ("hevc-annexb-au-v1".to_owned(), data, length)
        }
        "vp9" => ("vp9-frame-v1".to_owned(), Vec::new(), None),
        "av1" => (
            "av1-low-overhead-tu-v1".to_owned(),
            normalize_av1_extradata(&extradata)?,
            None,
        ),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("codec {codec:?} has no Vivid 1.5 portable packetization"),
            ));
        }
    };
    let (codec_string, decoder_config) = derive_video_description(
        &codec,
        container_box,
        &extradata,
        parameters.profile,
        parameters.level,
        parameters.bits_per_raw_sample,
    );
    let colorimetry_inferred = parameters.color_primaries == AVCOL_PRI_UNSPECIFIED
        || parameters.color_trc == AVCOL_TRC_UNSPECIFIED
        || parameters.color_space == AVCOL_SPC_UNSPECIFIED
        || parameters.color_range == AVCOL_RANGE_UNSPECIFIED;
    let color_primaries = map_primaries(parameters.color_primaries, height)?;
    let transfer = map_transfer(parameters.color_trc)?;
    let matrix = map_matrix(parameters.color_space, height)?;
    let range = map_range(parameters.color_range)?;
    let sar = parameters.sample_aspect_ratio;
    let (sar_num, sar_den) = if sar.num > 0 && sar.den > 0 {
        (sar.num as u32, sar.den as u32)
    } else {
        (1, 1)
    };

    Ok((
        VideoInfo {
            packetization,
            codec,
            extradata,
            width,
            height,
            profile: parameters.profile,
            level: parameters.level,
            bitrate: parameters.bit_rate,
            color_primaries,
            transfer,
            matrix,
            range,
            colorimetry_inferred,
            sar_num,
            sar_den,
            max_access_unit_bytes: 0,
            maximum_rate_millihertz: 0,
            maximum_encoded_bits_per_second: 0,
            maximum_records_per_second: 0,
            first_pts_us: None,
            duration_us: None,
            has_audio: false,
            audio: None,
            codec_string,
            decoder_config,
        },
        nal_length_size,
    ))
}

/// Derive the optional `decoder-description-v1` fields from the container decoder-configuration
/// box or codec parameters. For H.264 Annex-B extradata, use the parameter sets themselves.
/// Returns `None` rather than guessing: a missing description is valid, a wrong one is not.
fn derive_video_description(
    codec: &str,
    container_box: Option<Vec<u8>>,
    annexb_extradata: &[u8],
    profile: i32,
    level: i32,
    bits_per_raw_sample: i32,
) -> (Option<String>, Option<Vec<u8>>) {
    match codec {
        "h264" => {
            // avcC bytes 1..4 are exactly profile_idc, constraint flags, and level_idc.
            if let Some(avcc) = container_box.filter(|data| data.len() >= 4 && data[0] == 1) {
                let string = format!("avc1.{:02X}{:02X}{:02X}", avcc[1], avcc[2], avcc[3]);
                return (Some(string), Some(avcc));
            }
            // Annex-B extradata: the same three bytes follow the SPS NAL header.
            let string = find_h264_sps(annexb_extradata)
                .filter(|sps| sps.len() >= 4)
                .map(|sps| format!("avc1.{:02X}{:02X}{:02X}", sps[1], sps[2], sps[3]));
            (string, None)
        }
        "hevc" => {
            let Some(hvcc) = container_box.filter(|data| data.len() >= 23 && data[0] == 1) else {
                return (None, None);
            };
            (Some(hevc_codec_string(&hvcc)), Some(hvcc))
        }
        "vp9" => {
            if let Some(vpcc) = container_box.filter(|data| data.len() >= 12 && data[0] == 1) {
                let string = vp9_codec_string(
                    i32::from(vpcc[1]),
                    i32::from(vpcc[2]),
                    i32::from(vpcc[3] >> 4),
                );
                return (string, Some(vpcc));
            }
            (vp9_codec_string(profile, level, bits_per_raw_sample), None)
        }
        "av1" => {
            let Some(av1c) = container_box.filter(|data| data.len() >= 4 && data[0] == 0x81) else {
                return (None, None);
            };
            (Some(av1_codec_string(&av1c)), Some(av1c))
        }
        _ => (None, None),
    }
}

fn vp9_codec_string(profile: i32, level: i32, bits_per_raw_sample: i32) -> Option<String> {
    let profile = u8::try_from(profile).ok().filter(|profile| *profile <= 3)?;
    let level = u8::try_from(level).ok().filter(|level| {
        matches!(
            level,
            10 | 11 | 20 | 21 | 30 | 31 | 40 | 41 | 50 | 51 | 52 | 60 | 61 | 62
        )
    })?;
    let bit_depth = match bits_per_raw_sample {
        8 | 10 | 12 => bits_per_raw_sample,
        0 if profile <= 1 => 8,
        _ => return None,
    };
    Some(format!("vp09.{profile:02}.{level:02}.{bit_depth:02}"))
}

/// Find the first H.264 SPS NAL in an Annex-B buffer, returned from its header byte onward.
fn find_h264_sps(data: &[u8]) -> Option<&[u8]> {
    let mut index = 0;
    while index + 4 <= data.len() {
        if data[index..index + 3] == [0, 0, 1] {
            let nal = &data[index + 3..];
            if nal.first().is_some_and(|header| header & 0x1f == 7) {
                return Some(nal);
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    None
}

/// ISO/IEC 14496-15 Annex E codec string from an hvcC box body.
fn hevc_codec_string(hvcc: &[u8]) -> String {
    let profile_space = hvcc[1] >> 6;
    let tier = if hvcc[1] & 0x20 == 0 { 'L' } else { 'H' };
    let profile_idc = hvcc[1] & 0x1f;
    let compatibility = u32::from_be_bytes(hvcc[2..6].try_into().unwrap());
    let level_idc = hvcc[12];
    let mut string = String::from("hvc1.");
    if profile_space > 0 {
        string.push((b'A' + profile_space - 1) as char);
    }
    string.push_str(&format!(
        "{profile_idc}.{:X}.{tier}{level_idc}",
        compatibility.reverse_bits()
    ));
    // Constraint bytes, trailing zero bytes omitted.
    let constraints = &hvcc[6..12];
    let significant = constraints
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |index| index + 1);
    for byte in &constraints[..significant] {
        string.push_str(&format!(".{byte:X}"));
    }
    string
}

/// AV1 codec string from an av1C box body (AV1-ISOBMFF section 2.3).
fn av1_codec_string(av1c: &[u8]) -> String {
    let profile = av1c[1] >> 5;
    let level = av1c[1] & 0x1f;
    let tier = if av1c[2] & 0x80 == 0 { 'M' } else { 'H' };
    let high_bitdepth = av1c[2] & 0x40 != 0;
    let twelve_bit = av1c[2] & 0x20 != 0;
    let depth = match (high_bitdepth, twelve_bit) {
        (false, _) => 8,
        (true, false) => 10,
        (true, true) => 12,
    };
    format!("av01.{profile}.{level:02}{tier}.{depth:02}")
}

unsafe fn audio_info(
    parameters: *mut AVCodecParameters,
    stream: *mut AVStream,
) -> io::Result<AudioInfo> {
    let parameters = unsafe { &*parameters };
    let codec_pointer = unsafe { avcodec_get_name(parameters.codec_id) };
    let codec = if codec_pointer.is_null() {
        format!("ffmpeg-codec-{}", parameters.codec_id)
    } else {
        unsafe { CStr::from_ptr(codec_pointer) }
            .to_string_lossy()
            .into_owned()
    };
    let packetization = match codec.as_str() {
        "mp3" => "mp3-frame-v1",
        "aac" => "aac-raw-au-v1",
        "alac" => "alac-frame-v1",
        "opus" => vivid_protocol::media::AUDIO_PACKETIZATION_OPUS,
        "vorbis" => vivid_protocol::media::AUDIO_PACKETIZATION_VORBIS,
        "flac" => vivid_protocol::media::AUDIO_PACKETIZATION_FLAC,
        "pcm_u8" | "pcm_s16le" | "pcm_s24le" | "pcm_s32le" | "pcm_f32le" | "pcm_f64le"
        | "pcm_mulaw" | "pcm_alaw" => "pcm-packet-v1",
        _ => "unsupported-audio-packetization",
    };
    let extradata_size = usize::try_from(parameters.extradata_size)
        .ok()
        .filter(|size| *size <= MAX_EXTRADATA)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid audio extradata"))?;
    let extradata = if extradata_size == 0 {
        Vec::new()
    } else if parameters.extradata.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "audio extradata pointer is null",
        ));
    } else {
        unsafe { std::slice::from_raw_parts(parameters.extradata, extradata_size) }.to_vec()
    };
    let sample_rate = u32::try_from(parameters.sample_rate).unwrap_or(0);
    let channels = u16::try_from(parameters.ch_layout.nb_channels).unwrap_or(0);
    let extradata = normalize_audio_extradata(&codec, &extradata)?;
    if vivid_protocol::media::valid_audio_packetization(&codec, packetization) {
        vivid_protocol::media::validate_audio_initialization(
            &codec,
            packetization,
            &extradata,
            sample_rate,
            channels,
        )?;
    }
    let channel_mask = if matches!(parameters.ch_layout.order, 0 | 1) {
        parameters.ch_layout.mask
    } else {
        u64::MAX
    };
    let stream = unsafe { &*stream };
    let first_pts_us = (stream.start_time != AV_NOPTS_VALUE
        && stream.time_base.num > 0
        && stream.time_base.den > 0)
        .then(|| timestamp_us(stream.start_time, stream.time_base));
    let codec_string = derive_audio_codec_string(&codec, &extradata);
    Ok(AudioInfo {
        codec,
        packetization: packetization.into(),
        extradata,
        sample_rate,
        channels,
        channel_mask,
        bitrate: parameters.bit_rate,
        max_access_unit_bytes: 0,
        maximum_rate_millihertz: 0,
        maximum_encoded_bits_per_second: 0,
        maximum_records_per_second: 0,
        first_pts_us,
        duration_us: stream_duration_us(stream),
        codec_string,
    })
}

/// Derive the optional `decoder-description-v1` audio codec string.
fn derive_audio_codec_string(codec: &str, extradata: &[u8]) -> Option<String> {
    match codec {
        "aac" => {
            // `mp4a.40.<audioObjectType>` from the AudioSpecificConfig's leading bits.
            let first = *extradata.first()?;
            let object_type = if first >> 3 == 31 {
                // Five-bit escape: 32 plus a six-bit extension.
                let second = *extradata.get(1)?;
                32 + (u16::from(first & 0x07) << 3 | u16::from(second >> 5))
            } else {
                u16::from(first >> 3)
            };
            (object_type != 0).then(|| format!("mp4a.40.{object_type}"))
        }
        "mp3" => Some("mp3".to_owned()),
        "opus" | "vorbis" | "flac" | "alac" => Some(codec.to_owned()),
        "pcm_mulaw" => Some("ulaw".to_owned()),
        "pcm_alaw" => Some("alaw".to_owned()),
        "pcm_u8" => Some("pcm-u8".to_owned()),
        "pcm_s16le" => Some("pcm-s16".to_owned()),
        "pcm_s24le" => Some("pcm-s24".to_owned()),
        "pcm_s32le" => Some("pcm-s32".to_owned()),
        "pcm_f32le" => Some("pcm-f32".to_owned()),
        _ => None,
    }
}

fn normalize_audio_extradata(codec: &str, data: &[u8]) -> io::Result<Vec<u8>> {
    match codec {
        "opus" => {
            if data.starts_with(b"OpusHead") {
                Ok(data.to_vec())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Opus stream has no canonical OpusHead",
                ))
            }
        }
        "vorbis" => normalize_vorbis_extradata(data),
        "flac" => normalize_flac_extradata(data),
        _ => Ok(data.to_vec()),
    }
}

fn normalize_vorbis_extradata(data: &[u8]) -> io::Result<Vec<u8>> {
    if data.first() == Some(&2) {
        return Ok(data.to_vec());
    }
    if data.len() < 6 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Vorbis private data is truncated",
        ));
    }
    let lengths = [
        usize::from(u16::from_be_bytes(data[0..2].try_into().unwrap())),
        usize::from(u16::from_be_bytes(data[2..4].try_into().unwrap())),
        usize::from(u16::from_be_bytes(data[4..6].try_into().unwrap())),
    ];
    let total = lengths
        .iter()
        .try_fold(6_usize, |total, length| total.checked_add(*length))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Vorbis lengths overflow"))?;
    if total != data.len() || lengths.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported Vorbis private-data packing",
        ));
    }
    let mut output = vec![2];
    append_xiph_length(&mut output, lengths[0]);
    append_xiph_length(&mut output, lengths[1]);
    output.extend_from_slice(&data[6..]);
    Ok(output)
}

fn append_xiph_length(output: &mut Vec<u8>, mut length: usize) {
    while length >= 255 {
        output.push(255);
        length -= 255;
    }
    output.push(length as u8);
}

fn normalize_flac_extradata(data: &[u8]) -> io::Result<Vec<u8>> {
    if data.len() == 34 {
        return Ok(data.to_vec());
    }
    let block = if data.len() == 42 && &data[..4] == b"fLaC" {
        &data[4..]
    } else if data.len() == 38 {
        data
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "FLAC private data has no canonical STREAMINFO",
        ));
    };
    let block_type = block[0] & 0x7f;
    let block_length = u32::from_be_bytes([0, block[1], block[2], block[3]]) as usize;
    if block_type != 0 || block_length != 34 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "FLAC private data does not begin with STREAMINFO",
        ));
    }
    Ok(block[4..].to_vec())
}

fn normalize_h26x_extradata(data: &[u8], hevc: bool) -> io::Result<(Vec<u8>, Option<usize>)> {
    if data.is_empty() || data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]) {
        return Ok((data.to_vec(), None));
    }
    if data[0] != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported H.26x extradata",
        ));
    }
    let mut output = Vec::new();
    if hevc {
        if data.len() < 23 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated hvcC"));
        }
        let length_size = usize::from((data[21] & 3) + 1);
        let arrays = usize::from(data[22]);
        let mut offset = 23;
        for _ in 0..arrays {
            if offset + 3 > data.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "truncated hvcC array",
                ));
            }
            offset += 1;
            let count = usize::from(u16::from_be_bytes(
                data[offset..offset + 2].try_into().unwrap(),
            ));
            offset += 2;
            for _ in 0..count {
                append_config_nal(data, &mut offset, &mut output)?;
            }
        }
        if offset != data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trailing hvcC data",
            ));
        }
        Ok((output, Some(length_size)))
    } else {
        if data.len() < 7 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated avcC"));
        }
        let length_size = usize::from((data[4] & 3) + 1);
        let mut offset = 6;
        let sps = usize::from(data[5] & 0x1f);
        for _ in 0..sps {
            append_config_nal(data, &mut offset, &mut output)?;
        }
        if offset >= data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing avcC PPS count",
            ));
        }
        let pps = usize::from(data[offset]);
        offset += 1;
        for _ in 0..pps {
            append_config_nal(data, &mut offset, &mut output)?;
        }
        Ok((output, Some(length_size)))
    }
}

/// Convert an AV1CodecConfigurationRecord into the canonical sequence-header OBU carried by
/// Vivid. FFmpeg exposes Matroska/WebM CodecPrivate as av1C, while Vivid's low-overhead profile
/// deliberately excludes container framing.
fn normalize_av1_extradata(data: &[u8]) -> io::Result<Vec<u8>> {
    if data.is_empty() || data[0] & 0x80 == 0 {
        return Ok(data.to_vec());
    }
    if data.len() < 4 || data[0] != 0x81 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported AV1 codec configuration",
        ));
    }
    Ok(data[4..].to_vec())
}

fn append_config_nal(data: &[u8], offset: &mut usize, output: &mut Vec<u8>) -> io::Result<()> {
    if *offset + 2 > data.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated NAL length",
        ));
    }
    let length = usize::from(u16::from_be_bytes(
        data[*offset..*offset + 2].try_into().unwrap(),
    ));
    *offset += 2;
    let end = offset
        .checked_add(length)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "NAL exceeds extradata"))?;
    output.extend_from_slice(&[0, 0, 0, 1]);
    output.extend_from_slice(&data[*offset..end]);
    *offset = end;
    Ok(())
}

fn length_prefixed_to_annex_b(data: &[u8], length_size: usize) -> io::Result<Vec<u8>> {
    if !(1..=4).contains(&length_size) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid NAL length size",
        ));
    }
    let mut output = Vec::with_capacity(data.len() + 16);
    let mut offset = 0;
    while offset < data.len() {
        if offset + length_size > data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated access-unit NAL length",
            ));
        }
        let mut length = 0_usize;
        for byte in &data[offset..offset + length_size] {
            length = (length << 8) | usize::from(*byte);
        }
        offset += length_size;
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "access-unit NAL exceeds packet")
            })?;
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(&data[offset..end]);
        offset = end;
    }
    Ok(output)
}

fn map_primaries(value: c_int, height: u32) -> io::Result<u64> {
    match value {
        1 => Ok(1),
        5 => Ok(2),
        6 => Ok(3),
        9 => Ok(4),
        AVCOL_PRI_UNSPECIFIED => Ok(if height > 576 {
            1 // BT.709 for HD video.
        } else if height > 480 {
            2 // BT.601 625-line family.
        } else {
            3 // BT.601 525-line family.
        }),
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "video color primaries are missing or unsupported",
        )),
    }
}
fn map_transfer(value: c_int) -> io::Result<u64> {
    match value {
        1 => Ok(1),
        13 => Ok(2),
        AVCOL_TRC_UNSPECIFIED => Ok(1),
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "video transfer characteristic is missing or unsupported",
        )),
    }
}
fn map_matrix(value: c_int, height: u32) -> io::Result<u64> {
    match value {
        0 => Ok(0),
        1 => Ok(1),
        5 | 6 => Ok(2),
        9 => Ok(3),
        AVCOL_SPC_UNSPECIFIED => Ok(if height > 576 { 1 } else { 2 }),
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "video matrix coefficients are missing or unsupported",
        )),
    }
}
fn map_range(value: c_int) -> io::Result<u64> {
    match value {
        1 => Ok(1),
        2 => Ok(2),
        AVCOL_RANGE_UNSPECIFIED => Ok(1),
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "video signal range is missing or unsupported",
        )),
    }
}

fn timestamp_us(timestamp: i64, time_base: AVRational) -> i64 {
    if timestamp == AV_NOPTS_VALUE {
        return AV_NOPTS_VALUE;
    }
    let value = i128::from(timestamp)
        .saturating_mul(i128::from(time_base.num))
        .saturating_mul(1_000_000)
        / i128::from(time_base.den);
    value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn timestamp_duration_us(timestamp: i64, time_base: AVRational) -> u64 {
    if timestamp <= 0 {
        return 0;
    }
    u64::try_from(timestamp_us(timestamp, time_base)).unwrap_or(u64::MAX)
}

fn stream_duration_us(stream: &AVStream) -> Option<u64> {
    (stream.duration > 0).then(|| timestamp_duration_us(stream.duration, stream.time_base))
}

fn duration_between(first: Option<i64>, end: Option<i64>) -> Option<u64> {
    first
        .zip(end)
        .and_then(|(first, end)| u64::try_from(end.saturating_sub(first)).ok())
}

fn packet_end_pts(pts_us: i64, duration_us: u64) -> i64 {
    pts_us.saturating_add(i64::try_from(duration_us).unwrap_or(i64::MAX))
}

fn seek_context(
    context: *mut AVFormatContext,
    packet: *mut AVPacket,
    stream_index: c_int,
    time_base: AVRational,
    target_pts_us: i64,
) -> io::Result<()> {
    let timestamp = i128::from(target_pts_us).saturating_mul(i128::from(time_base.den))
        / i128::from(time_base.num).saturating_mul(1_000_000);
    let timestamp = timestamp.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    let result = unsafe { av_seek_frame(context, stream_index, timestamp, AVSEEK_FLAG_BACKWARD) };
    if result < 0 {
        return Err(ffmpeg_error("could not seek media", result));
    }
    unsafe {
        av_packet_unref(packet);
        avformat_flush(context);
    }
    Ok(())
}

fn packet_trim(packet: &AVPacket) -> (u32, u32) {
    let mut size = 0_usize;
    let data = unsafe { av_packet_get_side_data(packet, AV_PKT_DATA_SKIP_SAMPLES, &mut size) };
    if data.is_null() || size < 8 {
        return (0, 0);
    }
    let values = unsafe { std::slice::from_raw_parts(data, 8) };
    (
        u32::from_le_bytes(values[0..4].try_into().unwrap()),
        u32::from_le_bytes(values[4..8].try_into().unwrap()),
    )
}

fn ffmpeg_error(context: &str, code: c_int) -> io::Error {
    let mut buffer = [0_i8; 256];
    let description = if unsafe { av_strerror(code, buffer.as_mut_ptr(), buffer.len()) } == 0 {
        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    } else {
        format!("FFmpeg error {code}")
    };
    io::Error::other(format!("{context}: {description}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(
        target_pointer_width = "64",
        not(ffmpeg_old_channel_layout),
        ffmpeg_codecpar_has_framerate
    ))]
    #[test]
    fn ffmpeg_8_codec_parameters_color_layout_matches_abi() {
        assert_eq!(std::mem::offset_of!(AVCodecParameters, framerate), 88);
        assert_eq!(
            std::mem::offset_of!(AVCodecParameters, color_primaries),
            104
        );
    }

    #[test]
    fn timestamps_convert_to_microseconds() {
        assert_eq!(
            timestamp_us(
                90_000,
                AVRational {
                    num: 1,
                    den: 90_000
                }
            ),
            1_000_000
        );
        assert_eq!(
            timestamp_us(AV_NOPTS_VALUE, AVRational { num: 1, den: 1 }),
            i64::MIN
        );
    }

    #[test]
    fn unspecified_colorimetry_uses_sd_and_hd_defaults() {
        assert_eq!(map_primaries(AVCOL_PRI_UNSPECIFIED, 480).unwrap(), 3);
        assert_eq!(map_primaries(AVCOL_PRI_UNSPECIFIED, 576).unwrap(), 2);
        assert_eq!(map_primaries(AVCOL_PRI_UNSPECIFIED, 720).unwrap(), 1);
        assert_eq!(map_transfer(AVCOL_TRC_UNSPECIFIED).unwrap(), 1);
        assert_eq!(map_matrix(AVCOL_SPC_UNSPECIFIED, 480).unwrap(), 2);
        assert_eq!(map_matrix(AVCOL_SPC_UNSPECIFIED, 720).unwrap(), 1);
        assert_eq!(map_range(AVCOL_RANGE_UNSPECIFIED).unwrap(), 1);
    }

    #[test]
    fn declared_unsupported_colorimetry_still_fails() {
        assert!(map_primaries(22, 1080).is_err());
        assert!(map_transfer(22).is_err());
        assert!(map_matrix(22, 1080).is_err());
        assert!(map_range(22).is_err());
    }

    #[test]
    fn portable_audio_private_data_is_canonicalized() {
        let mut legacy_vorbis = Vec::new();
        legacy_vorbis.extend_from_slice(&3_u16.to_be_bytes());
        legacy_vorbis.extend_from_slice(&4_u16.to_be_bytes());
        legacy_vorbis.extend_from_slice(&5_u16.to_be_bytes());
        legacy_vorbis.extend_from_slice(b"abcdefghijkl");
        assert_eq!(
            normalize_vorbis_extradata(&legacy_vorbis).unwrap(),
            [vec![2, 3, 4], b"abcdefghijkl".to_vec()].concat()
        );

        let streaminfo = [0x5a; 34];
        let mut flac = b"fLaC".to_vec();
        flac.extend_from_slice(&[0x80, 0, 0, 34]);
        flac.extend_from_slice(&streaminfo);
        assert_eq!(normalize_flac_extradata(&flac).unwrap(), streaminfo);

        assert!(normalize_audio_extradata("opus", b"not-an-opus-head").is_err());
    }

    #[test]
    fn av1_container_private_data_is_canonicalized() {
        let av1c = [
            0x81, 0x04, 0x0c, 0x00, 0x0a, 0x0e, 0x00, 0x00, 0x00, 0x24, 0xc4, 0xff, 0xdf, 0x30,
            0xbf, 0x44, 0x04, 0x04, 0x04, 0x10,
        ];
        assert_eq!(normalize_av1_extradata(&av1c).unwrap(), av1c[4..]);
        assert_eq!(normalize_av1_extradata(&av1c[4..]).unwrap(), av1c[4..]);
        assert!(normalize_av1_extradata(&[0x82, 0, 0, 0]).is_err());
    }

    #[test]
    fn decoder_descriptions_cover_container_and_parameter_driven_codecs() {
        let vpcc = vec![1, 2, 31, 10 << 4, 1, 1, 1, 0, 0, 0, 0, 0];
        let (codec_string, decoder_config) =
            derive_video_description("vp9", Some(vpcc.clone()), &[], 0, 0, 0);
        assert_eq!(codec_string.as_deref(), Some("vp09.02.31.10"));
        assert_eq!(decoder_config.as_deref(), Some(vpcc.as_slice()));

        let (codec_string, decoder_config) = derive_video_description("vp9", None, &[], 0, 41, 0);
        assert_eq!(codec_string.as_deref(), Some("vp09.00.41.08"));
        assert_eq!(decoder_config, None);
        assert_eq!(vp9_codec_string(2, 41, 0), None);
    }

    #[test]
    fn finite_claims_measure_peak_one_second_windows() {
        let mut claims = RateClaims::default();
        claims.observe(0, 10).unwrap();
        claims.observe(100_000, 20).unwrap();
        claims.observe(1_100_000, 7).unwrap();
        assert_eq!(claims.maximum_records, 2);
        assert_eq!(claims.maximum_bytes, 30);
    }
}
