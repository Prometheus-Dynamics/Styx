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
use crate::{
    Codec, CodecDescriptor, CodecError, CodecKind, DEFAULT_CODEC_POOL_CHUNK_BYTES,
    DEFAULT_CODEC_POOL_SPARE,
};

use super::util::{
    SendSyncScalingContext, bytes_per_pixel, fourcc_for_pixel_format, init_ffmpeg,
    layouts_for_frame, pixel_format_for_fourcc,
};

mod drm_prime;
use drm_prime::{
    FfmpegDrmPrimeBacking, configure_drm_prime_decoder_context, drm_prime_descriptor_from_frame,
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
    fn pool_from_limits(pool_limits: Option<(usize, usize, usize)>) -> BufferPool {
        let (_min, max, spare) =
            pool_limits.unwrap_or((2, DEFAULT_CODEC_POOL_CHUNK_BYTES, DEFAULT_CODEC_POOL_SPARE));
        BufferPool::lazy(max, spare)
    }

    // Constructor mirrors FFmpeg decoder policy fields; a builder would add ceremony here.
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
        Ok(Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input,
                output,
                name,
                impl_name,
            },
            codec,
            pool: Self::pool_from_limits(pool_limits),
            thread_count,
            state: Mutex::new(None),
            zero_copy,
            tolerant,
            strip_app,
        })
    }

    // Named-decoder variant intentionally mirrors `new` for explicit backend selection.
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
        Ok(Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input,
                output,
                name,
                impl_name,
            },
            codec,
            pool: Self::pool_from_limits(pool_limits),
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
        if self.zero_copy {
            let _ = unsafe { configure_drm_prime_decoder_context(&mut context, self.codec) };
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

    #[cfg(target_os = "linux")]
    fn decode_shared(
        &self,
        data: &[u8],
        timestamp: u64,
        color: ColorSpace,
        pool: &SharedBufferPool,
    ) -> Result<FrameLease, CodecError> {
        let mut lock = self
            .state
            .lock()
            .map_err(|e| CodecError::Codec(format!("ffmpeg decoder lock poisoned: {e}")))?;
        let state = self.decoder_state(&mut lock)?;
        self.feed_packet_shared(state, data, timestamp, color, pool)?;
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

    #[cfg(target_os = "linux")]
    fn feed_packet_shared(
        &self,
        state: &mut DecoderState,
        data: &[u8],
        timestamp: u64,
        color: ColorSpace,
        pool: &SharedBufferPool,
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
                self.drain_frames_shared(state, timestamp, color, pool)?;
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
        self.drain_frames_shared(state, timestamp, color, pool)
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

    #[cfg(target_os = "linux")]
    fn drain_frames_shared(
        &self,
        state: &mut DecoderState,
        timestamp: u64,
        color: ColorSpace,
        pool: &SharedBufferPool,
    ) -> Result<(), CodecError> {
        loop {
            let mut frame = FfFrame::empty();
            match state.decoder.receive_frame(&mut frame) {
                Ok(()) => self.queue_frame_shared(state, frame, timestamp, color, pool)?,
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
            if frame.format() == PixelFormat::DRM_PRIME {
                self.queue_drm_prime_frame(state, frame, resolution, ts, color)?;
                return Ok(());
            }
            let target_fmt = pixel_format_for_fourcc(self.descriptor.output)
                .ok_or_else(|| CodecError::Codec("unsupported output pixel format".into()))?;
            let converted = if frame.format() == target_fmt {
                frame
            } else {
                // External zero-copy output must own the converted FFmpeg frame because the
                // scaler scratch buffer is reused on the next decode.
                self.scale_frame_cached_ref(&mut state.scaler, &frame, target_fmt)?
                    .clone()
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
        let src = if frame.format() == target_fmt {
            &frame
        } else {
            self.scale_frame_cached_ref(&mut state.scaler, &frame, target_fmt)?
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

    #[cfg(target_os = "linux")]
    fn queue_frame_shared(
        &self,
        state: &mut DecoderState,
        frame: FfFrame,
        timestamp: u64,
        color: ColorSpace,
        pool: &SharedBufferPool,
    ) -> Result<(), CodecError> {
        let width = frame.width();
        let height = frame.height();
        let ts = frame
            .timestamp()
            .map(|t| t.max(0) as u64)
            .unwrap_or(timestamp);
        let resolution = Resolution::new(width, height)
            .ok_or_else(|| CodecError::Codec("invalid decoded resolution".into()))?;
        if self.zero_copy && frame.format() == PixelFormat::DRM_PRIME {
            self.queue_drm_prime_frame(state, frame, resolution, ts, color)?;
            return Ok(());
        }

        let target_fmt = pixel_format_for_fourcc(self.descriptor.output)
            .ok_or_else(|| CodecError::Codec("unsupported output pixel format".into()))?;
        let src = if frame.format() == target_fmt {
            &frame
        } else {
            self.scale_frame_cached_ref(&mut state.scaler, &frame, target_fmt)?
        };
        let bpp = bytes_per_pixel(target_fmt)
            .ok_or_else(|| CodecError::Codec("unsupported packed format".into()))?;
        let stride = src.stride(0) as usize;
        let row_len = width as usize * bpp;
        let required = row_len.saturating_mul(height as usize);
        let mut lease = pool
            .lease()
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        lease
            .try_resize(required)
            .map_err(|err| CodecError::Codec(err.to_string()))?;
        for y in 0..height as usize {
            let src_off = y * stride;
            let dst_off = y * row_len;
            let src_data = src.data(0);
            if src_off + row_len <= src_data.len() && dst_off + row_len <= lease.len() {
                lease.as_mut_slice()[dst_off..dst_off + row_len]
                    .copy_from_slice(&src_data[src_off..src_off + row_len]);
            }
        }
        state.queued.push_back(
            FrameLease::single_plane_shared(
                FrameMeta::new(
                    MediaFormat::new(self.descriptor.output, resolution, ColorSpace::Srgb),
                    ts,
                ),
                lease,
                required,
                row_len,
            )
            .map_err(|err| CodecError::Codec(err.to_string()))?,
        );
        Ok(())
    }

    fn queue_drm_prime_frame(
        &self,
        state: &mut DecoderState,
        frame: FfFrame,
        resolution: Resolution,
        timestamp: u64,
        color: ColorSpace,
    ) -> Result<(), CodecError> {
        let descriptor = unsafe { drm_prime_descriptor_from_frame(&frame) }?;
        let actual_fourcc = descriptor.format;
        if actual_fourcc != self.descriptor.output {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.output,
                actual: actual_fourcc,
            });
        }
        let layouts: smallvec::SmallVec<[PlaneLayout; 3]> = descriptor
            .planes
            .iter()
            .map(|plane| PlaneLayout {
                offset: plane.offset,
                len: plane.len,
                stride: plane.stride,
            })
            .collect();
        let backing = Arc::new(FfmpegDrmPrimeBacking {
            _frame: frame,
            planes: descriptor.planes,
            backing_bytes: descriptor.backing_bytes,
        });
        state.queued.push_back(FrameLease::from_external(
            FrameMeta::new(
                MediaFormat::new(actual_fourcc, resolution, color),
                timestamp,
            )
            .with_residency(FrameResidency::Dmabuf),
            layouts,
            backing,
        ));
        Ok(())
    }

    fn scale_frame_cached_ref<'a>(
        &self,
        cache: &'a mut Option<ScalingCache>,
        src: &FfFrame,
        target: PixelFormat,
    ) -> Result<&'a FfFrame, CodecError> {
        let can_reuse = cache.as_ref().is_some_and(|cached| {
            cached.width == src.width()
                && cached.height == src.height()
                && cached.src_fmt == src.format()
                && cached.dst_fmt == target
        });
        if can_reuse {
            let cached = cache.as_mut().expect("cache checked above");
            cached
                .scaler
                .0
                .run(src, &mut cached.scratch)
                .map_err(|e| CodecError::Codec(e.to_string()))?;
            return Ok(&cached.scratch);
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
        Ok(&cache.as_ref().unwrap().scratch)
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

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
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
        self.decode_shared(
            plane.data(),
            input.meta().timestamp,
            input.meta().format.color,
            pool,
        )
        .map(Some)
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

    fn backing_bytes(&self) -> Option<usize> {
        Some((0..3).map(|idx| self.frame.data(idx).len()).sum())
    }

    fn backing_kind(&self) -> &'static str {
        "ffmpeg_frame"
    }
}

#[path = "decoder_codecs.rs"]
mod decoder_codecs;
pub use decoder_codecs::{FfmpegH264Decoder, FfmpegH265Decoder, FfmpegMjpegDecoder};
