use std::sync::Mutex;

use ffmpeg_next::util::error::EAGAIN;
use ffmpeg_next::{
    codec::{self, Id},
    encoder,
    error::Error as FfmpegError,
    frame::Video as FfFrame,
    packet::Packet,
    software::scaling::{context::Context as ScalingContext, flag::Flags},
    util::format::pixel::Pixel as PixelFormat,
};
use styx_core::prelude::*;

use crate::{Codec, CodecDescriptor, CodecError, CodecKind};

use super::util::{SendSyncScalingContext, init_ffmpeg, pixel_format_for_fourcc};

/// Shared encoder state.
struct EncoderState {
    encoder: encoder::video::Encoder,
    scaler: Option<SendSyncScalingContext>,
    src_format: PixelFormat,
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    src_frame: FfFrame,
    dst_frame: Option<FfFrame>,
    next_pts: i64,
    queued: std::collections::VecDeque<Vec<u8>>,
}

/// FFmpeg encoder for H.264/H.265/MJPEG.
pub struct FfmpegVideoEncoder {
    descriptor: CodecDescriptor,
    codec: ffmpeg_next::Codec,
    pool: BufferPool,
    state: Mutex<Option<EncoderState>>,
    opts: Mutex<FfmpegEncoderOptions>,
}

impl FfmpegVideoEncoder {
    pub fn new(
        id: Id,
        name: &'static str,
        impl_name: &'static str,
        input: FourCc,
        output: FourCc,
        opts: FfmpegEncoderOptions,
    ) -> Result<Self, CodecError> {
        init_ffmpeg()?;
        let codec = codec::encoder::find(id)
            .ok_or_else(|| CodecError::Codec(format!("ffmpeg encoder {id:?} not found")))?;
        let (pool_min, pool_max, pool_spare) = opts.pool_limits.unwrap_or((2, 1 << 20, 4));
        Ok(Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Encoder,
                input,
                output,
                name,
                impl_name,
            },
            codec,
            pool: BufferPool::with_limits(pool_min, pool_max, pool_spare),
            state: Mutex::new(None),
            opts: Mutex::new(opts),
        })
    }

    pub fn new_by_name(
        codec_name: &str,
        name: &'static str,
        impl_name: &'static str,
        input: FourCc,
        output: FourCc,
        opts: FfmpegEncoderOptions,
    ) -> Result<Self, CodecError> {
        init_ffmpeg()?;
        let codec = codec::encoder::find_by_name(codec_name).ok_or_else(|| {
            CodecError::Codec(format!("ffmpeg encoder {codec_name} not found"))
        })?;
        let (pool_min, pool_max, pool_spare) = opts.pool_limits.unwrap_or((2, 1 << 20, 4));
        Ok(Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Encoder,
                input,
                output,
                name,
                impl_name,
            },
            codec,
            pool: BufferPool::with_limits(pool_min, pool_max, pool_spare),
            state: Mutex::new(None),
            opts: Mutex::new(opts),
        })
    }

    fn ensure_state(
        &self,
        src_width: u32,
        src_height: u32,
        dst_width: u32,
        dst_height: u32,
    ) -> Result<(), CodecError> {
        let src_format = pixel_format_for_fourcc(self.descriptor.input).ok_or_else(|| {
            CodecError::Codec(format!(
                "ffmpeg encoder input format {} not supported",
                self.descriptor.input
            ))
        })?;
        let mut guard = self
            .state
            .lock()
            .map_err(|e| CodecError::Codec(format!("ffmpeg encoder lock poisoned: {e}")))?;
        let opts = *self
            .opts
            .lock()
            .map_err(|e| CodecError::Codec(format!("ffmpeg opts lock poisoned: {e}")))?;
        let needs_reinit = guard
            .as_ref()
            .map(|s| {
                s.src_format != src_format
                    || s.src_width != src_width
                    || s.src_height != src_height
                    || s.dst_width != dst_width
                    || s.dst_height != dst_height
            })
            .unwrap_or(true);
        if !needs_reinit {
            return Ok(());
        }

        let ctx = codec::Context::new_with_codec(self.codec);
        let mut enc_ctx = ctx
            .encoder()
            .video()
            .map_err(|e| CodecError::Codec(e.to_string()))?;
        if let Some(threads) = opts.thread_count {
            enc_ctx.set_threading(codec::threading::Config {
                kind: codec::threading::Type::Frame,
                count: threads,
            });
        }
        enc_ctx.set_width(dst_width);
        enc_ctx.set_height(dst_height);
        let dst_format = pick_encoder_pixel_format(self.descriptor.output, src_format, self.codec)
            .ok_or_else(|| {
                CodecError::Codec("ffmpeg encoder has no supported pixel formats".into())
            })?;
        enc_ctx.set_format(dst_format);
        if let Some((num, den)) = opts.framerate {
            enc_ctx.set_time_base((den as i32, num as i32));
            enc_ctx.set_frame_rate(Some((num as i32, den as i32)));
        } else {
            // Default to a high-FPS timebase so mem2mem encoders don't assume 30fps.
            enc_ctx.set_time_base((1, 120));
            enc_ctx.set_frame_rate(Some((120, 1)));
        }
        let bitrate = usize::try_from(opts.bitrate).unwrap_or(usize::MAX);
        enc_ctx.set_bit_rate(bitrate);
        // Hardware encoders (v4l2m2m) often require low-latency settings.
        enc_ctx.set_max_b_frames(0);
        if let Some(gop) = opts.gop {
            let gop_u32: u32 = gop.try_into().unwrap_or(u32::MAX);
            enc_ctx.set_gop(gop_u32);
        } else {
            // Default to a 1-second GOP at the default timebase (120fps).
            enc_ctx.set_gop(120);
        }
        let encoder = enc_ctx
            .open_as(self.codec)
            .map_err(|e| CodecError::Codec(format!("ffmpeg open encoder failed: {e}")))?;

        let needs_scaler =
            src_format != dst_format || src_width != dst_width || src_height != dst_height;
        let scaler = if needs_scaler {
            Some(SendSyncScalingContext(
                ScalingContext::get(
                    src_format,
                    src_width,
                    src_height,
                    dst_format,
                    dst_width,
                    dst_height,
                    Flags::FAST_BILINEAR,
                )
                .map_err(|e| CodecError::Codec(format!("ffmpeg scaler init failed: {e}")))?,
            ))
        } else {
            None
        };
        let src_frame = alloc_video_frame(src_format, src_width, src_height)?;
        let dst_frame = if needs_scaler {
            Some(alloc_video_frame(dst_format, dst_width, dst_height)?)
        } else {
            None
        };

        *guard = Some(EncoderState {
            encoder,
            scaler,
            src_format,
            src_width,
            src_height,
            dst_width,
            dst_height,
            src_frame,
            dst_frame,
            next_pts: 0,
            queued: std::collections::VecDeque::new(),
        });
        Ok(())
    }

    fn encode(&self, frame: &FrameLease) -> Result<FrameLease, CodecError> {
        let meta = frame.meta();
        let src_width = meta.format.resolution.width.get();
        let src_height = meta.format.resolution.height.get();
        let (dst_width, dst_height) = self.target_resolution(src_width, src_height);

        self.ensure_state(src_width, src_height, dst_width, dst_height)?;
        let mut guard = self
            .state
            .lock()
            .map_err(|e| CodecError::Codec(format!("ffmpeg encoder lock poisoned: {e}")))?;
        let state = guard
            .as_mut()
            .ok_or_else(|| CodecError::Codec("ffmpeg encoder state missing".into()))?;
        self.push_frame(state, frame)?;
        let data = state
            .queued
            .pop_front()
            .ok_or(CodecError::Backpressure)?;
        let mut meta = meta.clone();
        if let Some(res) = Resolution::new(dst_width, dst_height) {
            meta.format = MediaFormat::new(meta.format.code, res, meta.format.color);
        }
        drop(guard);
        Ok(self.packet_to_frame(&meta, data))
    }

    fn push_frame(&self, state: &mut EncoderState, frame: &FrameLease) -> Result<(), CodecError> {
        let pts = state.next_pts;
        state.next_pts = state.next_pts.saturating_add(1);
        write_input_frame(&mut state.src_frame, state.src_format, frame)?;
        state.src_frame.set_pts(Some(pts));
        if state.scaler.is_some() {
            {
                let scaler = state
                    .scaler
                    .as_mut()
                    .ok_or_else(|| CodecError::Codec("ffmpeg scaler missing".into()))?;
                let dst_frame = state
                    .dst_frame
                    .as_mut()
                    .ok_or_else(|| CodecError::Codec("ffmpeg dst frame missing".into()))?;
                scaler
                    .0
                    .run(&state.src_frame, dst_frame)
                    .map_err(|e| CodecError::Codec(format!("ffmpeg scale failed: {e}")))?;
                dst_frame.set_pts(Some(pts));
            }

            let send_result = {
                let dst = state
                    .dst_frame
                    .as_ref()
                    .ok_or_else(|| CodecError::Codec("ffmpeg dst frame missing".into()))?;
                state.encoder.send_frame(dst)
            };
            match send_result {
                Ok(()) => {}
                Err(err) if is_again(&err) => {
                    self.drain_packets(state)?;
                    let dst = state
                        .dst_frame
                        .as_ref()
                        .ok_or_else(|| CodecError::Codec("ffmpeg dst frame missing".into()))?;
                    state
                        .encoder
                        .send_frame(dst)
                        .map_err(|e| CodecError::Codec(format!("ffmpeg send_frame failed: {e}")))?;
                }
                Err(err) => return Err(CodecError::Codec(format!("ffmpeg send_frame failed: {err}"))),
            }
        } else {
            let send_result = state.encoder.send_frame(&state.src_frame);
            match send_result {
                Ok(()) => {}
                Err(err) if is_again(&err) => {
                    self.drain_packets(state)?;
                    state
                        .encoder
                        .send_frame(&state.src_frame)
                        .map_err(|e| CodecError::Codec(format!("ffmpeg send_frame failed: {e}")))?;
                }
                Err(err) => return Err(CodecError::Codec(format!("ffmpeg send_frame failed: {err}"))),
            }
        }
        self.drain_packets(state)
    }

    fn drain_packets(&self, state: &mut EncoderState) -> Result<(), CodecError> {
        loop {
            let mut packet = Packet::empty();
            match state.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    let data = packet
                        .data()
                        .ok_or_else(|| CodecError::Codec("ffmpeg packet missing data".into()))?;
                    state.queued.push_back(data.to_vec());
                }
                Err(err) if is_again(&err) => break,
                Err(FfmpegError::Eof) => break,
                Err(err) => return Err(CodecError::Codec(format!("ffmpeg receive_packet failed: {err}"))),
            }
        }
        Ok(())
    }

    fn packet_to_frame(&self, meta: &FrameMeta, data: Vec<u8>) -> FrameLease {
        let mut buf = self.pool.lease();
        buf.resize(data.len());
        buf.as_mut_slice().copy_from_slice(&data);
        FrameLease::single_plane(
            FrameMeta::new(
                MediaFormat::new(
                    self.descriptor.output,
                    meta.format.resolution,
                    meta.format.color,
                ),
                meta.timestamp,
            ),
            buf,
            data.len(),
            data.len(),
        )
    }

    /// Encode a frame and return every produced packet (streaming-friendly).
    pub fn encode_all(&self, frame: &FrameLease) -> Result<Vec<FrameLease>, CodecError> {
        let mut meta = frame.meta().clone();
        let src_width = meta.format.resolution.width.get();
        let src_height = meta.format.resolution.height.get();
        let (dst_width, dst_height) = self.target_resolution(src_width, src_height);
        if let Some(res) = Resolution::new(dst_width, dst_height) {
            meta.format = MediaFormat::new(meta.format.code, res, meta.format.color);
        }
        self.ensure_state(src_width, src_height, dst_width, dst_height)?;
        let mut lock = self
            .state
            .lock()
            .map_err(|e| CodecError::Codec(format!("ffmpeg encoder lock poisoned: {e}")))?;
        let state = lock
            .as_mut()
            .ok_or_else(|| CodecError::Codec("ffmpeg encoder state missing".into()))?;
        self.push_frame(state, frame)?;
        let mut out = Vec::new();
        while let Some(pkt) = state.queued.pop_front() {
            out.push(self.packet_to_frame(&meta, pkt));
        }
        Ok(out)
    }

    /// Flush the encoder, returning any delayed packets.
    pub fn flush_encoder(&self, meta: &FrameMeta) -> Result<Vec<FrameLease>, CodecError> {
        let mut lock = self
            .state
            .lock()
            .map_err(|e| CodecError::Codec(format!("ffmpeg encoder lock poisoned: {e}")))?;
        let Some(state) = lock.as_mut() else {
            return Ok(Vec::new());
        };
        state
            .encoder
            .send_eof()
            .map_err(|e| CodecError::Codec(e.to_string()))?;
        self.drain_packets(state)?;
        let mut out = Vec::new();
        while let Some(pkt) = state.queued.pop_front() {
            out.push(self.packet_to_frame(meta, pkt));
        }
        Ok(out)
    }

    fn target_resolution(&self, src_width: u32, src_height: u32) -> (u32, u32) {
        if let Ok(opts) = self.opts.lock() {
            if let Some(res) = opts.output_resolution {
                return (res.width.get(), res.height.get());
            }
        }
        (src_width, src_height)
    }
}

impl Codec for FfmpegVideoEncoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        if input.meta().format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: input.meta().format.code,
            });
        }
        self.encode(&input)
    }
}

pub struct FfmpegMjpegEncoder(pub FfmpegVideoEncoder);

impl FfmpegMjpegEncoder {
    pub fn new_rgb24() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::MJPEG,
            "mjpeg",
            "ffmpeg",
            FourCc::new(*b"RG24"),
            FourCc::new(*b"MJPG"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn new_nv12() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::MJPEG,
            "mjpeg",
            "ffmpeg",
            FourCc::new(*b"NV12"),
            FourCc::new(*b"MJPG"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn with_options(opts: FfmpegEncoderOptions) -> Result<Self, CodecError> {
        Self::with_options_for_input(FourCc::new(*b"RG24"), opts)
    }

    pub fn with_options_for_input(
        input: FourCc,
        opts: FfmpegEncoderOptions,
    ) -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::MJPEG,
            "mjpeg",
            "ffmpeg",
            input,
            FourCc::new(*b"MJPG"),
            opts,
        )
        .map(Self)
    }
}

impl Codec for FfmpegMjpegEncoder {
    fn descriptor(&self) -> &CodecDescriptor {
        self.0.descriptor()
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.0.process(input)
    }
}

pub struct FfmpegH264Encoder(pub FfmpegVideoEncoder);

impl FfmpegH264Encoder {
    pub fn new_rgb24() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::H264,
            "h264",
            "ffmpeg",
            FourCc::new(*b"RG24"),
            FourCc::new(*b"H264"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn new_nv12() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::H264,
            "h264",
            "ffmpeg",
            FourCc::new(*b"NV12"),
            FourCc::new(*b"H264"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn new_v4l2m2m_rgb24() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new_by_name(
            "h264_v4l2m2m",
            "h264",
            "h264_v4l2m2m",
            FourCc::new(*b"RG24"),
            FourCc::new(*b"H264"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn new_v4l2m2m_nv12() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new_by_name(
            "h264_v4l2m2m",
            "h264",
            "h264_v4l2m2m",
            FourCc::new(*b"NV12"),
            FourCc::new(*b"H264"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn with_options(opts: FfmpegEncoderOptions) -> Result<Self, CodecError> {
        Self::with_options_for_input(FourCc::new(*b"RG24"), opts)
    }

    pub fn with_options_for_input(
        input: FourCc,
        opts: FfmpegEncoderOptions,
    ) -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::H264,
            "h264",
            "ffmpeg",
            input,
            FourCc::new(*b"H264"),
            opts,
        )
        .map(Self)
    }
}

impl Codec for FfmpegH264Encoder {
    fn descriptor(&self) -> &CodecDescriptor {
        self.0.descriptor()
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.0.process(input)
    }
}

pub struct FfmpegH265Encoder(pub FfmpegVideoEncoder);

impl FfmpegH265Encoder {
    pub fn new_rgb24() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::HEVC,
            "h265",
            "ffmpeg",
            FourCc::new(*b"RG24"),
            FourCc::new(*b"H265"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn new_nv12() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::HEVC,
            "h265",
            "ffmpeg",
            FourCc::new(*b"NV12"),
            FourCc::new(*b"H265"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn new_v4l2m2m_rgb24() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new_by_name(
            "hevc_v4l2m2m",
            "h265",
            "hevc_v4l2m2m",
            FourCc::new(*b"RG24"),
            FourCc::new(*b"H265"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn new_v4l2m2m_nv12() -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new_by_name(
            "hevc_v4l2m2m",
            "h265",
            "hevc_v4l2m2m",
            FourCc::new(*b"NV12"),
            FourCc::new(*b"H265"),
            FfmpegEncoderOptions::default(),
        )
        .map(Self)
    }

    pub fn with_options(opts: FfmpegEncoderOptions) -> Result<Self, CodecError> {
        Self::with_options_for_input(FourCc::new(*b"RG24"), opts)
    }

    pub fn with_options_for_input(
        input: FourCc,
        opts: FfmpegEncoderOptions,
    ) -> Result<Self, CodecError> {
        FfmpegVideoEncoder::new(
            Id::HEVC,
            "h265",
            "ffmpeg",
            input,
            FourCc::new(*b"H265"),
            opts,
        )
        .map(Self)
    }
}

impl Codec for FfmpegH265Encoder {
    fn descriptor(&self) -> &CodecDescriptor {
        self.0.descriptor()
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.0.process(input)
    }
}

/// Basic tuning knobs for FFmpeg encoders.
#[derive(Clone, Copy, Debug)]
pub struct FfmpegEncoderOptions {
    /// Target bitrate in bits per second.
    pub bitrate: u64,
    /// Optional GOP size.
    pub gop: Option<i32>,
    /// Optional frame rate (num, den).
    pub framerate: Option<(u32, u32)>,
    /// Optional thread count.
    pub thread_count: Option<usize>,
    /// Optional buffer pool limits (min leases, max bytes, max outstanding).
    pub pool_limits: Option<(usize, usize, usize)>,
    /// Optional output resolution override; scales from input to this size.
    pub output_resolution: Option<Resolution>,
}

impl Default for FfmpegEncoderOptions {
    fn default() -> Self {
        Self {
            bitrate: 20_000_000,
            gop: None,
            framerate: None,
            thread_count: None,
            pool_limits: None,
            output_resolution: None,
        }
    }
}

impl FfmpegVideoEncoder {
    /// Update bitrate at runtime; applies to the live encoder and persists for future frames.
    pub fn set_bitrate(&self, bitrate: u64) {
        if bitrate == 0 {
            return;
        }
        if let Ok(mut guard) = self.state.lock() {
            if let Some(state) = guard.as_mut() {
                state
                    .encoder
                    .set_bit_rate(usize::try_from(bitrate).unwrap_or(usize::MAX));
            }
        }
        if let Ok(mut opts) = self.opts.lock() {
            opts.bitrate = bitrate;
        }
    }

    /// Update GOP size.
    pub fn set_gop(&self, gop: Option<i32>) {
        if let Ok(mut locked) = self.opts.lock() {
            locked.gop = gop;
        }
        if let Some(g) = gop {
            if let Ok(mut guard) = self.state.lock() {
                if let Some(state) = guard.as_mut() {
                    let gop_u32: u32 = g.try_into().unwrap_or(u32::MAX);
                    state.encoder.set_gop(gop_u32);
                }
            }
        }
    }

    /// Update frame rate (num, den).
    pub fn set_framerate(&self, framerate: Option<(u32, u32)>) {
        if let Ok(mut locked) = self.opts.lock() {
            locked.framerate = framerate;
        }
        if let Some((num, den)) = framerate {
            if let Ok(mut guard) = self.state.lock() {
                if let Some(state) = guard.as_mut() {
                    state.encoder.set_time_base((den as i32, num as i32));
                }
            }
        }
    }

    /// Override the output resolution (scales from input). Setting to `None` uses input resolution.
    pub fn set_output_resolution(&self, res: Option<Resolution>) {
        if let Ok(mut locked) = self.opts.lock() {
            locked.output_resolution = res;
        }
        // Force re-init on next frame to pick up new geometry.
        if let Ok(mut guard) = self.state.lock() {
            *guard = None;
        }
    }
}

fn pick_encoder_pixel_format(
    output: FourCc,
    input: PixelFormat,
    codec: ffmpeg_next::Codec,
) -> Option<PixelFormat> {
    let video = codec.video().ok()?;
    let supported: Vec<PixelFormat> = video
        .formats()
        .map(|it| it.collect::<Vec<_>>())
        .unwrap_or_default();
    if supported.is_empty() {
        // Some wrappers (notably V4L2 mem2mem) don't always report pixel formats through
        // libavcodec, but they still accept standard YUV inputs. Default to NV12 for video,
        // and a JPEG-friendly format for MJPEG.
        return Some(if output == FourCc::new(*b"MJPG") {
            PixelFormat::YUVJ420P
        } else {
            PixelFormat::NV12
        });
    }

    if supported.contains(&input) {
        return Some(input);
    }

    if output == FourCc::new(*b"MJPG") {
        for candidate in [
            PixelFormat::YUVJ420P,
            PixelFormat::YUVJ422P,
            PixelFormat::YUV420P,
            PixelFormat::NV12,
        ] {
            if supported.contains(&candidate) {
                return Some(candidate);
            }
        }
    } else {
        for candidate in [
            PixelFormat::NV12,
            PixelFormat::YUV420P,
            PixelFormat::YUVJ420P,
        ] {
            if supported.contains(&candidate) {
                return Some(candidate);
            }
        }
    }

    supported.first().copied()
}

fn alloc_video_frame(fmt: PixelFormat, width: u32, height: u32) -> Result<FfFrame, CodecError> {
    let mut frame = FfFrame::empty();
    frame.set_format(fmt);
    frame.set_width(width);
    frame.set_height(height);
    unsafe {
        frame.alloc(fmt, width, height);
    }
    Ok(frame)
}

fn write_input_frame(
    dst: &mut FfFrame,
    fmt: PixelFormat,
    frame: &FrameLease,
) -> Result<(), CodecError> {
    let meta = frame.meta();
    let width = meta.format.resolution.width.get();
    let height = meta.format.resolution.height.get();
    if dst.width() != width || dst.height() != height {
        return Err(CodecError::Codec(
            "ffmpeg input frame geometry mismatch".into(),
        ));
    }
    let planes = frame.planes();

    match fmt {
        PixelFormat::RGB24 | PixelFormat::BGR24 => {
            let plane = planes
                .into_iter()
                .next()
                .ok_or_else(|| CodecError::Codec("RGB24 missing plane".into()))?;
            let src_stride = plane.stride();
            let src = plane.data();
            let dst_stride = dst.stride(0);
            let dst_data = dst.data_mut(0);
            let row_bytes = width as usize * 3;
            for y in 0..height as usize {
                let src_off = y * src_stride;
                let dst_off = y * dst_stride;
                if src_off + row_bytes > src.len() || dst_off + row_bytes > dst_data.len() {
                    return Err(CodecError::Codec("RGB24 plane too short".into()));
                }
                dst_data[dst_off..dst_off + row_bytes]
                    .copy_from_slice(&src[src_off..src_off + row_bytes]);
            }
            Ok(())
        }
        PixelFormat::RGBA | PixelFormat::BGRA => {
            let plane = planes
                .into_iter()
                .next()
                .ok_or_else(|| CodecError::Codec("RGBA missing plane".into()))?;
            let src_stride = plane.stride();
            let src = plane.data();
            let dst_stride = dst.stride(0);
            let dst_data = dst.data_mut(0);
            let row_bytes = width as usize * 4;
            for y in 0..height as usize {
                let src_off = y * src_stride;
                let dst_off = y * dst_stride;
                if src_off + row_bytes > src.len() || dst_off + row_bytes > dst_data.len() {
                    return Err(CodecError::Codec("RGBA plane too short".into()));
                }
                dst_data[dst_off..dst_off + row_bytes]
                    .copy_from_slice(&src[src_off..src_off + row_bytes]);
            }
            Ok(())
        }
        PixelFormat::NV12 => {
            if planes.len() < 2 {
                return Err(CodecError::Codec("NV12 requires 2 planes".into()));
            }
            let y = &planes[0];
            let uv = &planes[1];
            let (w, h) = (width as usize, height as usize);
            if y.data().len() < y.stride().saturating_mul(h) {
                return Err(CodecError::Codec("NV12 Y plane too short".into()));
            }
            if uv.data().len() < uv.stride().saturating_mul(h / 2) {
                return Err(CodecError::Codec("NV12 UV plane too short".into()));
            }

            let dst_y_stride = dst.stride(0);
            let dst_uv_stride = dst.stride(1);
            {
                let dst_y = dst.data_mut(0);
                for row in 0..h {
                    let src_row = &y.data()[row * y.stride()..row * y.stride() + w];
                    let dst_row = &mut dst_y[row * dst_y_stride..row * dst_y_stride + w];
                    dst_row.copy_from_slice(src_row);
                }
            }
            {
                let dst_uv = dst.data_mut(1);
                for row in 0..(h / 2) {
                    let src_row = &uv.data()[row * uv.stride()..row * uv.stride() + w];
                    let dst_row = &mut dst_uv[row * dst_uv_stride..row * dst_uv_stride + w];
                    dst_row.copy_from_slice(src_row);
                }
            }
            Ok(())
        }
        PixelFormat::YUV420P | PixelFormat::YUVJ420P => {
            if planes.len() < 3 {
                return Err(CodecError::Codec("I420 requires 3 planes".into()));
            }
            let y = &planes[0];
            let u = &planes[1];
            let v = &planes[2];
            let (w, h) = (width as usize, height as usize);
            let cw = w / 2;
            if y.data().len() < y.stride().saturating_mul(h) {
                return Err(CodecError::Codec("I420 Y plane too short".into()));
            }
            if u.data().len() < u.stride().saturating_mul(h / 2)
                || v.data().len() < v.stride().saturating_mul(h / 2)
            {
                return Err(CodecError::Codec("I420 UV plane too short".into()));
            }

            let dst_y_stride = dst.stride(0);
            let dst_u_stride = dst.stride(1);
            let dst_v_stride = dst.stride(2);
            {
                let dst_y = dst.data_mut(0);
                for row in 0..h {
                    let src_row = &y.data()[row * y.stride()..row * y.stride() + w];
                    let dst_row = &mut dst_y[row * dst_y_stride..row * dst_y_stride + w];
                    dst_row.copy_from_slice(src_row);
                }
            }
            {
                let dst_u = dst.data_mut(1);
                for row in 0..(h / 2) {
                    let src_u = &u.data()[row * u.stride()..row * u.stride() + cw];
                    let dst_u_row = &mut dst_u[row * dst_u_stride..row * dst_u_stride + cw];
                    dst_u_row.copy_from_slice(src_u);
                }
            }
            {
                let dst_v = dst.data_mut(2);
                for row in 0..(h / 2) {
                    let src_v = &v.data()[row * v.stride()..row * v.stride() + cw];
                    let dst_v_row = &mut dst_v[row * dst_v_stride..row * dst_v_stride + cw];
                    dst_v_row.copy_from_slice(src_v);
                }
            }
            Ok(())
        }
        PixelFormat::YUYV422 => {
            let plane = planes
                .first()
                .ok_or_else(|| CodecError::Codec("YUYV missing plane".into()))?;
            let (w, h) = (width as usize, height as usize);
            let bytes_per_row = w.saturating_mul(2);
            if plane.data().len() < plane.stride().saturating_mul(h) {
                return Err(CodecError::Codec("YUYV plane too short".into()));
            }

            let dst_stride = dst.stride(0);
            let dst_data = dst.data_mut(0);
            for row in 0..h {
                let src_row =
                    &plane.data()[row * plane.stride()..row * plane.stride() + bytes_per_row];
                let dst_row = &mut dst_data[row * dst_stride..row * dst_stride + bytes_per_row];
                dst_row.copy_from_slice(src_row);
            }
            Ok(())
        }
        _ => Err(CodecError::Codec(format!(
            "unsupported ffmpeg input pixel format: {fmt:?}"
        ))),
    }
}

fn is_again(err: &FfmpegError) -> bool {
    matches!(err, FfmpegError::Other { errno } if *errno == EAGAIN)
}
