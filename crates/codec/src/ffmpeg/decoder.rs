use std::{
    borrow::Cow,
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use ffmpeg_next::{
    codec::{self, Id},
    decoder,
    error::Error as FfmpegError,
    frame::Video as FfFrame,
    packet::Packet,
    software::scaling::flag::Flags,
    util::error::EAGAIN,
    util::format::pixel::Pixel as PixelFormat,
};
use styx_core::prelude::*;

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, frame_to_dynamic_image};
use crate::{Codec, CodecDescriptor, CodecError, CodecKind};

use super::util::{
    SendSyncScalingContext, bytes_per_pixel, fourcc_for_pixel_format, init_ffmpeg,
    layouts_for_frame, pixel_format_for_fourcc,
};

/// Generic FFmpeg video decoder to RGB24.
pub struct FfmpegVideoDecoder {
    descriptor: CodecDescriptor,
    codec: ffmpeg_next::Codec,
    pool: BufferPool,
    thread_count: Option<usize>,
    state: Mutex<Option<DecoderState>>,
    zero_copy: bool,
    tolerant: bool,
    strip_app: bool,
}

struct DecoderState {
    decoder: decoder::Video,
    scaler: Option<ScalingCache>,
    queued: VecDeque<FrameLease>,
}

struct ScalingCache {
    src_fmt: PixelFormat,
    dst_fmt: PixelFormat,
    width: u32,
    height: u32,
    scaler: SendSyncScalingContext,
    scratch: FfFrame,
}

impl FfmpegVideoDecoder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Id,
        name: &'static str,
        impl_name: &'static str,
        input: FourCc,
        output: FourCc,
        zero_copy: bool,
        thread_count: Option<usize>,
        pool_limits: Option<(usize, usize, usize)>,
        tolerant: bool,
        strip_app: bool,
    ) -> Result<Self, CodecError> {
        init_ffmpeg()?;
        let codec = codec::decoder::find(id)
            .ok_or_else(|| CodecError::Codec(format!("ffmpeg codec {id:?} not found")))?;
        let (min, max, spare) = pool_limits.unwrap_or((2, 1 << 20, 4));
        Ok(Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input,
                output,
                name,
                impl_name,
            },
            codec,
            pool: BufferPool::with_limits(min, max, spare),
            thread_count,
            state: Mutex::new(None),
            zero_copy,
            tolerant,
            strip_app,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_by_name(
        decoder_name: &'static str,
        name: &'static str,
        impl_name: &'static str,
        input: FourCc,
        output: FourCc,
        zero_copy: bool,
        thread_count: Option<usize>,
        pool_limits: Option<(usize, usize, usize)>,
        tolerant: bool,
        strip_app: bool,
    ) -> Result<Self, CodecError> {
        init_ffmpeg()?;
        let codec = codec::decoder::find_by_name(decoder_name)
            .ok_or_else(|| CodecError::Codec(format!("ffmpeg decoder {decoder_name} not found")))?;
        let (min, max, spare) = pool_limits.unwrap_or((2, 1 << 20, 4));
        Ok(Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input,
                output,
                name,
                impl_name,
            },
            codec,
            pool: BufferPool::with_limits(min, max, spare),
            thread_count,
            state: Mutex::new(None),
            zero_copy,
            tolerant,
            strip_app,
        })
    }

    fn prepare_decoder_state(&self) -> Result<DecoderState, CodecError> {
        let mut context = codec::Context::new_with_codec(self.codec);
        if let Some(threads) = self.thread_count {
            context.set_threading(codec::threading::Config {
                kind: codec::threading::Type::Frame,
                count: threads,
            });
        }
        let decoder = context
            .decoder()
            .video()
            .map_err(|e| CodecError::Codec(e.to_string()))?;
        Ok(DecoderState {
            decoder,
            scaler: None,
            queued: VecDeque::new(),
        })
    }

    fn decoder_state<'a>(
        &self,
        guard: &'a mut Option<DecoderState>,
    ) -> Result<&'a mut DecoderState, CodecError> {
        if guard.is_none() {
            *guard = Some(self.prepare_decoder_state()?);
        }
        Ok(guard.as_mut().expect("state must exist"))
    }

    fn decode(
        &self,
        data: &[u8],
        timestamp: u64,
        color: ColorSpace,
    ) -> Result<FrameLease, CodecError> {
        let mut lock = self
            .state
            .lock()
            .map_err(|e| CodecError::Codec(format!("ffmpeg decoder lock poisoned: {e}")))?;
        let state = self.decoder_state(&mut lock)?;
        self.feed_packet(state, data, timestamp, color)?;
        state
            .queued
            .pop_front()
            .ok_or_else(|| CodecError::Codec("ffmpeg decoder produced no frame".into()))
    }

    fn feed_packet(
        &self,
        state: &mut DecoderState,
        data: &[u8],
        timestamp: u64,
        color: ColorSpace,
    ) -> Result<(), CodecError> {
        let cleaned = if self.strip_app {
            strip_app_segments(data)
        } else {
            Cow::Borrowed(data)
        };
        let packet = Packet::copy(&cleaned);
        match state.decoder.send_packet(&packet) {
            Ok(()) => {}
            Err(err) if is_again(&err) => {
                self.drain_frames(state, timestamp, color)?;
                state
                    .decoder
                    .send_packet(&packet)
                    .map_err(|e| CodecError::Codec(e.to_string()))?;
            }
            Err(_) => {
                *state = self.prepare_decoder_state()?;
                let retry = state.decoder.send_packet(&packet);
                if let Err(e) = retry {
                    if self.tolerant {
                        return Ok(());
                    }
                    return Err(CodecError::Codec(e.to_string()));
                }
            }
        }
        self.drain_frames(state, timestamp, color)
    }

    fn drain_frames(
        &self,
        state: &mut DecoderState,
        timestamp: u64,
        color: ColorSpace,
    ) -> Result<(), CodecError> {
        loop {
            let mut frame = FfFrame::empty();
            match state.decoder.receive_frame(&mut frame) {
                Ok(()) => self.queue_frame(state, frame, timestamp, color)?,
                Err(err) if is_again(&err) => break,
                Err(FfmpegError::Eof) => break,
                Err(err) => {
                    *state = self.prepare_decoder_state()?;
                    if self.tolerant {
                        continue;
                    }
                    return Err(CodecError::Codec(err.to_string()));
                }
            }
        }
        Ok(())
    }

    fn queue_frame(
        &self,
        state: &mut DecoderState,
        frame: FfFrame,
        timestamp: u64,
        color: ColorSpace,
    ) -> Result<(), CodecError> {
        let width = frame.width();
        let height = frame.height();
        let ts = frame
            .timestamp()
            .map(|t| t.max(0) as u64)
            .unwrap_or(timestamp);
        let resolution = Resolution::new(width, height)
            .ok_or_else(|| CodecError::Codec("invalid decoded resolution".into()))?;
        if self.zero_copy {
            let target_fmt = pixel_format_for_fourcc(self.descriptor.output)
                .ok_or_else(|| CodecError::Codec("unsupported output pixel format".into()))?;
            let converted = if frame.format() == target_fmt {
                frame
            } else {
                self.scale_frame_cached(&mut state.scaler, &frame, target_fmt)?
            };
            let pixfmt = converted.format();
            let actual_fourcc = fourcc_for_pixel_format(pixfmt).ok_or_else(|| {
                CodecError::Codec(format!("unsupported ffmpeg pixel format {pixfmt:?}"))
            })?;
            if actual_fourcc != self.descriptor.output {
                return Err(CodecError::FormatMismatch {
                    expected: self.descriptor.output,
                    actual: actual_fourcc,
                });
            }
            let layouts = layouts_for_frame(pixfmt, &converted)
                .ok_or_else(|| CodecError::Codec("unsupported ffmpeg layout".into()))?;
            let backing = Arc::new(FfmpegBacking { frame: converted });
            state.queued.push_back(FrameLease::from_external(
                FrameMeta::new(MediaFormat::new(actual_fourcc, resolution, color), ts),
                layouts,
                backing,
            ));
            return Ok(());
        }

        let target_fmt = pixel_format_for_fourcc(self.descriptor.output)
            .ok_or_else(|| CodecError::Codec("unsupported output pixel format".into()))?;
        let src_owned;
        let src = if frame.format() == target_fmt {
            &frame
        } else {
            src_owned = self.scale_frame_cached(&mut state.scaler, &frame, target_fmt)?;
            &src_owned
        };
        let bpp = bytes_per_pixel(target_fmt)
            .ok_or_else(|| CodecError::Codec("unsupported packed format".into()))?;
        let stride = src.stride(0) as usize;
        let row_len = width as usize * bpp;
        let required = row_len.saturating_mul(height as usize);
        let mut buf = self.pool.lease();
        buf.resize(required);
        for y in 0..height as usize {
            let src_off = y * stride;
            let dst_off = y * row_len;
            let src_data = src.data(0);
            if src_off + row_len <= src_data.len() && dst_off + row_len <= buf.len() {
                buf.as_mut_slice()[dst_off..dst_off + row_len]
                    .copy_from_slice(&src_data[src_off..src_off + row_len]);
            }
        }
        state.queued.push_back(FrameLease::single_plane(
            FrameMeta::new(
                MediaFormat::new(self.descriptor.output, resolution, ColorSpace::Srgb),
                ts,
            ),
            buf,
            required,
            row_len,
        ));
        Ok(())
    }

    fn scale_frame_cached(
        &self,
        cache: &mut Option<ScalingCache>,
        src: &FfFrame,
        target: PixelFormat,
    ) -> Result<FfFrame, CodecError> {
        if let Some(cached) = cache {
            if cached.width == src.width()
                && cached.height == src.height()
                && cached.src_fmt == src.format()
                && cached.dst_fmt == target
            {
                cached
                    .scaler
                    .0
                    .run(src, &mut cached.scratch)
                    .map_err(|e| CodecError::Codec(e.to_string()))?;
                return Ok(cached.scratch.clone());
            }
        }

        let mut scaler = ffmpeg_next::software::scaling::context::Context::get(
            src.format(),
            src.width(),
            src.height(),
            target,
            src.width(),
            src.height(),
            Flags::BILINEAR,
        )
        .map_err(|e| CodecError::Codec(e.to_string()))?;
        let mut scratch = FfFrame::empty();
        scratch.set_format(target);
        scratch.set_width(src.width());
        scratch.set_height(src.height());
        unsafe {
            scratch.alloc(target, src.width(), src.height());
        }
        scaler
            .run(src, &mut scratch)
            .map_err(|e| CodecError::Codec(e.to_string()))?;
        *cache = Some(ScalingCache {
            src_fmt: src.format(),
            dst_fmt: target,
            width: src.width(),
            height: src.height(),
            scaler: SendSyncScalingContext(scaler),
            scratch,
        });
        Ok(cache.as_ref().unwrap().scratch.clone())
    }

    /// Decode a packet and return all produced frames (useful for streaming callers).
    pub fn decode_all(
        &self,
        data: &[u8],
        timestamp: u64,
        color: ColorSpace,
    ) -> Result<Vec<FrameLease>, CodecError> {
        let mut lock = self
            .state
            .lock()
            .map_err(|e| CodecError::Codec(format!("ffmpeg decoder lock poisoned: {e}")))?;
        let state = self.decoder_state(&mut lock)?;
        self.feed_packet(state, data, timestamp, color)?;
        Ok(state.queued.drain(..).collect())
    }

    /// Flush the decoder and return any buffered frames.
    pub fn flush(&self, timestamp: u64, color: ColorSpace) -> Result<Vec<FrameLease>, CodecError> {
        let mut lock = self
            .state
            .lock()
            .map_err(|e| CodecError::Codec(format!("ffmpeg decoder lock poisoned: {e}")))?;
        let Some(state) = lock.as_mut() else {
            return Ok(Vec::new());
        };
        state
            .decoder
            .send_eof()
            .map_err(|e| CodecError::Codec(e.to_string()))?;
        self.drain_frames(state, timestamp, color)?;
        Ok(state.queued.drain(..).collect())
    }
}

fn strip_app_segments(data: &[u8]) -> Cow<'_, [u8]> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return Cow::Borrowed(data);
    }
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&data[..2]); // SOI
    let mut pos = 2usize;
    while pos + 3 < data.len() {
        if data[pos] != 0xFF {
            // Not a marker, bail out and return original.
            return Cow::Borrowed(data);
        }
        let marker = data[pos + 1];
        pos += 2;
        // End of image or start of scan: copy the rest and finish.
        if marker == 0xD9 || marker == 0xDA {
            out.extend_from_slice(&data[pos - 2..]);
            return Cow::Owned(out);
        }
        if pos + 2 > data.len() {
            return Cow::Borrowed(data);
        }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        if len < 2 || pos + len > data.len() {
            return Cow::Borrowed(data);
        }
        let seg_start = pos - 2;
        let seg_end = pos + len;
        let is_app = (0xE0..=0xEF).contains(&marker);
        if !is_app {
            out.extend_from_slice(&data[seg_start..seg_end]);
        }
        pos = seg_end;
    }
    Cow::Owned(out)
}

fn is_again(err: &FfmpegError) -> bool {
    matches!(err, FfmpegError::Other { errno } if *errno == EAGAIN)
}

impl Codec for FfmpegVideoDecoder {
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
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("ffmpeg decoder frame missing plane".into()))?;
        self.decode(
            plane.data(),
            input.meta().timestamp,
            input.meta().format.color,
        )
    }
}

#[cfg(feature = "image")]
impl ImageDecode for FfmpegVideoDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        let decoded = self.process(frame)?;
        frame_to_dynamic_image(&decoded).ok_or_else(|| {
            CodecError::Codec("unable to convert ffmpeg frame to DynamicImage".into())
        })
    }
}

struct FfmpegBacking {
    frame: FfFrame,
}

impl ExternalBacking for FfmpegBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        match index {
            0 => Some(self.frame.data(0)),
            1 => Some(self.frame.data(1)),
            2 => Some(self.frame.data(2)),
            _ => None,
        }
    }
}

/// MJPEG decoder via FFmpeg.
pub struct FfmpegMjpegDecoder(pub FfmpegVideoDecoder);

impl FfmpegMjpegDecoder {
    pub fn new_rgb24() -> Result<Self, CodecError> {
        Self::new_rgb24_for_input(FourCc::new(*b"MJPG"))
    }

    pub fn new_rgb24_for_input(input: FourCc) -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::MJPEG,
            "mjpeg",
            "ffmpeg",
            input,
            FourCc::new(*b"RG24"),
            false,
            None,
            None,
            true,
            true,
        )
        .map(Self)
    }

    pub fn new_nv12_zero_copy() -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::MJPEG,
            "mjpeg",
            "ffmpeg",
            FourCc::new(*b"MJPG"),
            FourCc::new(*b"NV12"),
            true,
            None,
            None,
            true,
            true,
        )
        .map(Self)
    }

    pub fn with_options(
        zero_copy: bool,
        threads: Option<usize>,
        pool_limits: Option<(usize, usize, usize)>,
    ) -> Result<Self, CodecError> {
        Self::with_options_for_input(FourCc::new(*b"MJPG"), zero_copy, threads, pool_limits)
    }

    pub fn with_options_for_input(
        input: FourCc,
        zero_copy: bool,
        threads: Option<usize>,
        pool_limits: Option<(usize, usize, usize)>,
    ) -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::MJPEG,
            "mjpeg",
            "ffmpeg",
            input,
            FourCc::new(*b"RG24"),
            zero_copy,
            threads,
            pool_limits,
            true,
            true,
        )
        .map(Self)
    }
}

impl Codec for FfmpegMjpegDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        self.0.descriptor()
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.0.process(input)
    }
}

#[cfg(feature = "image")]
impl ImageDecode for FfmpegMjpegDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        self.0.decode_image(frame)
    }
}

/// H.264 decoder via FFmpeg.
pub struct FfmpegH264Decoder(pub FfmpegVideoDecoder);

impl FfmpegH264Decoder {
    pub fn new_rgb24() -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::H264,
            "h264",
            "ffmpeg",
            FourCc::new(*b"H264"),
            FourCc::new(*b"RG24"),
            false,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }

    pub fn new_nv12_zero_copy() -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::H264,
            "h264",
            "ffmpeg",
            FourCc::new(*b"H264"),
            FourCc::new(*b"NV12"),
            true,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }

    pub fn with_options(
        zero_copy: bool,
        threads: Option<usize>,
        pool_limits: Option<(usize, usize, usize)>,
    ) -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::H264,
            "h264",
            "ffmpeg",
            FourCc::new(*b"H264"),
            FourCc::new(*b"RG24"),
            zero_copy,
            threads,
            pool_limits,
            false,
            false,
        )
        .map(Self)
    }

    pub fn new_v4l2request_nv12_zero_copy() -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new_by_name(
            "h264_v4l2request",
            "h264",
            "h264_v4l2request",
            FourCc::new(*b"H264"),
            FourCc::new(*b"NV12"),
            true,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }
}

impl Codec for FfmpegH264Decoder {
    fn descriptor(&self) -> &CodecDescriptor {
        self.0.descriptor()
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.0.process(input)
    }
}

#[cfg(feature = "image")]
impl ImageDecode for FfmpegH264Decoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        self.0.decode_image(frame)
    }
}

/// H.265/HEVC decoder via FFmpeg.
pub struct FfmpegH265Decoder(pub FfmpegVideoDecoder);

impl FfmpegH265Decoder {
    pub fn new_rgb24() -> Result<Self, CodecError> {
        Self::new_rgb24_for_input(FourCc::new(*b"H265"))
    }

    pub fn new_rgb24_for_input(input: FourCc) -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::HEVC,
            "h265",
            "ffmpeg",
            input,
            FourCc::new(*b"RG24"),
            false,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }

    pub fn new_nv12_zero_copy() -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::HEVC,
            "h265",
            "ffmpeg",
            FourCc::new(*b"H265"),
            FourCc::new(*b"NV12"),
            true,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }

    pub fn with_options(
        zero_copy: bool,
        threads: Option<usize>,
        pool_limits: Option<(usize, usize, usize)>,
    ) -> Result<Self, CodecError> {
        Self::with_options_for_input(FourCc::new(*b"H265"), zero_copy, threads, pool_limits)
    }

    pub fn with_options_for_input(
        input: FourCc,
        zero_copy: bool,
        threads: Option<usize>,
        pool_limits: Option<(usize, usize, usize)>,
    ) -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new(
            Id::HEVC,
            "h265",
            "ffmpeg",
            input,
            FourCc::new(*b"RG24"),
            zero_copy,
            threads,
            pool_limits,
            false,
            false,
        )
        .map(Self)
    }

    pub fn new_v4l2request_nv12_zero_copy() -> Result<Self, CodecError> {
        FfmpegVideoDecoder::new_by_name(
            "hevc_v4l2request",
            "h265",
            "hevc_v4l2request",
            FourCc::new(*b"H265"),
            FourCc::new(*b"NV12"),
            true,
            None,
            None,
            false,
            false,
        )
        .map(Self)
    }
}

impl Codec for FfmpegH265Decoder {
    fn descriptor(&self) -> &CodecDescriptor {
        self.0.descriptor()
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.0.process(input)
    }
}

#[cfg(feature = "image")]
impl ImageDecode for FfmpegH265Decoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        self.0.decode_image(frame)
    }
}
