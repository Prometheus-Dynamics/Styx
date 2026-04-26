use std::{
    borrow::Cow,
    collections::VecDeque,
    os::fd::{FromRawFd, OwnedFd},
    ptr,
    sync::{Arc, Mutex},
};

use ffmpeg_next::{
    codec::{self, Id},
    decoder,
    error::Error as FfmpegError,
    frame::Video as FfFrame,
    packet::Packet,
    software::scaling::flag::Flags,
    sys as ffi,
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
    fn pool_from_limits(pool_limits: Option<(usize, usize, usize)>) -> BufferPool {
        let (_min, max, spare) = pool_limits.unwrap_or((2, 1 << 20, 4));
        BufferPool::lazy(max, spare)
    }

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

    fn scale_frame_cached(
        &self,
        cache: &mut Option<ScalingCache>,
        src: &FfFrame,
        target: PixelFormat,
    ) -> Result<FfFrame, CodecError> {
        if let Some(cached) = cache
            && cached.width == src.width()
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

unsafe fn configure_drm_prime_decoder_context(
    context: &mut codec::Context,
    codec: ffmpeg_next::Codec,
) -> Result<(), CodecError> {
    if !unsafe { codec_supports_drm_prime_device_ctx(codec) } {
        return Err(CodecError::Codec(
            "ffmpeg decoder does not advertise DRM PRIME hw output".into(),
        ));
    }
    let mut device_ctx: *mut ffi::AVBufferRef = ptr::null_mut();
    let ret = unsafe {
        ffi::av_hwdevice_ctx_create(
            &mut device_ctx,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
            ptr::null(),
            ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        return Err(CodecError::Codec(format!(
            "ffmpeg DRM device creation failed: {}",
            FfmpegError::from(ret)
        )));
    }
    if device_ctx.is_null() {
        return Err(CodecError::Codec(
            "ffmpeg DRM device creation returned null".into(),
        ));
    }
    let ctx = unsafe { context.as_mut_ptr() };
    unsafe {
        (*ctx).hw_device_ctx = device_ctx;
        (*ctx).get_format = Some(prefer_drm_prime_format);
    }
    Ok(())
}

unsafe fn codec_supports_drm_prime_device_ctx(codec: ffmpeg_next::Codec) -> bool {
    let mut idx = 0;
    loop {
        let config = unsafe { ffi::avcodec_get_hw_config(codec.as_ptr(), idx) };
        if config.is_null() {
            return false;
        }
        let config = unsafe { &*config };
        let has_device_ctx = (config.methods
            & ffi::_bindgen_ty_4::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32)
            != 0;
        if has_device_ctx
            && config.device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM
            && config.pix_fmt == ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME
        {
            return true;
        }
        idx += 1;
    }
}

unsafe extern "C" fn prefer_drm_prime_format(
    _ctx: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    if formats.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }
    let mut idx = 0usize;
    let mut first = ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    loop {
        let fmt = unsafe { *formats.add(idx) };
        if fmt == ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            return first;
        }
        if idx == 0 {
            first = fmt;
        }
        if fmt == ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME {
            return fmt;
        }
        idx += 1;
    }
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

#[repr(C)]
#[derive(Clone, Copy)]
struct AvDrmObjectDescriptor {
    fd: i32,
    size: usize,
    format_modifier: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AvDrmPlaneDescriptor {
    object_index: i32,
    offset: isize,
    pitch: isize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AvDrmLayerDescriptor {
    format: u32,
    nb_planes: i32,
    planes: [AvDrmPlaneDescriptor; 4],
}

#[repr(C)]
struct AvDrmFrameDescriptor {
    nb_objects: i32,
    objects: [AvDrmObjectDescriptor; 4],
    nb_layers: i32,
    layers: [AvDrmLayerDescriptor; 4],
}

#[derive(Clone, Debug)]
struct DrmPrimePlane {
    fd: i32,
    offset: usize,
    len: usize,
    stride: usize,
}

struct DrmPrimeDescriptor {
    format: FourCc,
    planes: Vec<DrmPrimePlane>,
    backing_bytes: usize,
}

struct FfmpegDrmPrimeBacking {
    _frame: FfFrame,
    planes: Vec<DrmPrimePlane>,
    backing_bytes: usize,
}

impl ExternalBacking for FfmpegDrmPrimeBacking {
    fn plane_data(&self, _index: usize) -> Option<&[u8]> {
        None
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.backing_bytes)
    }

    fn backing_kind(&self) -> &'static str {
        "ffmpeg_drm_prime"
    }

    fn residency(&self) -> FrameResidency {
        FrameResidency::Dmabuf
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        let mut planes = Vec::with_capacity(self.planes.len());
        for plane in &self.planes {
            let fd = unsafe { libc::dup(plane.fd) };
            if fd < 0 {
                return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
            }
            planes.push(FrameFdPlane {
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
                offset: plane.offset,
                len: plane.len,
            });
        }
        Ok(Some(FrameBackingExport::DmabufPlanes { planes }))
    }
}

unsafe fn drm_prime_descriptor_from_frame(
    frame: &FfFrame,
) -> Result<DrmPrimeDescriptor, CodecError> {
    let av_frame = unsafe { frame.as_ptr() };
    let ptr = unsafe { (*av_frame).data[0] };
    if ptr.is_null() {
        return Err(CodecError::Codec(
            "ffmpeg DRM PRIME frame missing descriptor".into(),
        ));
    }
    let desc = unsafe { &*(ptr.cast::<AvDrmFrameDescriptor>()) };
    drm_prime_descriptor_from_raw(desc, frame.height() as usize)
}

fn drm_prime_descriptor_from_raw(
    desc: &AvDrmFrameDescriptor,
    height: usize,
) -> Result<DrmPrimeDescriptor, CodecError> {
    if desc.nb_objects <= 0 || desc.nb_objects as usize > desc.objects.len() {
        return Err(CodecError::Codec(
            "ffmpeg DRM PRIME descriptor has invalid object count".into(),
        ));
    }
    if desc.nb_layers <= 0 || desc.nb_layers as usize > desc.layers.len() {
        return Err(CodecError::Codec(
            "ffmpeg DRM PRIME descriptor has invalid layer count".into(),
        ));
    }
    let layer = desc.layers[0];
    if layer.nb_planes <= 0 || layer.nb_planes as usize > layer.planes.len() {
        return Err(CodecError::Codec(
            "ffmpeg DRM PRIME descriptor has invalid plane count".into(),
        ));
    }
    let format = FourCc::new(layer.format.to_le_bytes());
    let object_count = desc.nb_objects as usize;
    let mut planes = Vec::with_capacity(layer.nb_planes as usize);
    for idx in 0..layer.nb_planes as usize {
        let plane = layer.planes[idx];
        if plane.object_index < 0 || plane.object_index as usize >= object_count {
            return Err(CodecError::Codec(
                "ffmpeg DRM PRIME plane references invalid object".into(),
            ));
        }
        if plane.offset < 0 || plane.pitch <= 0 {
            return Err(CodecError::Codec(
                "ffmpeg DRM PRIME plane has invalid layout".into(),
            ));
        }
        let object = desc.objects[plane.object_index as usize];
        let offset = plane.offset as usize;
        let stride = plane.pitch as usize;
        if offset > object.size {
            return Err(CodecError::Codec(
                "ffmpeg DRM PRIME plane offset exceeds object size".into(),
            ));
        }
        let estimated = stride.saturating_mul(drm_plane_height(format, idx, height));
        let available = object.size.saturating_sub(offset);
        planes.push(DrmPrimePlane {
            fd: object.fd,
            offset,
            len: estimated.min(available),
            stride,
        });
    }
    let backing_bytes = desc.objects[..object_count]
        .iter()
        .map(|object| object.size)
        .sum();
    Ok(DrmPrimeDescriptor {
        format,
        planes,
        backing_bytes,
    })
}

fn drm_plane_height(format: FourCc, index: usize, height: usize) -> usize {
    match (&format.to_u32().to_le_bytes(), index) {
        (b"NV12" | b"NV21", 1) => height.div_ceil(2),
        (b"YU12" | b"YV12", 1 | 2) => height.div_ceil(2),
        _ => height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_prime_descriptor_parses_nv12_layout() {
        let desc = AvDrmFrameDescriptor {
            nb_objects: 1,
            objects: [
                AvDrmObjectDescriptor {
                    fd: -1,
                    size: 4096,
                    format_modifier: 0,
                },
                AvDrmObjectDescriptor {
                    fd: -1,
                    size: 0,
                    format_modifier: 0,
                },
                AvDrmObjectDescriptor {
                    fd: -1,
                    size: 0,
                    format_modifier: 0,
                },
                AvDrmObjectDescriptor {
                    fd: -1,
                    size: 0,
                    format_modifier: 0,
                },
            ],
            nb_layers: 1,
            layers: [
                AvDrmLayerDescriptor {
                    format: FourCc::new(*b"NV12").to_u32(),
                    nb_planes: 2,
                    planes: [
                        AvDrmPlaneDescriptor {
                            object_index: 0,
                            offset: 0,
                            pitch: 640,
                        },
                        AvDrmPlaneDescriptor {
                            object_index: 0,
                            offset: 2048,
                            pitch: 640,
                        },
                        AvDrmPlaneDescriptor {
                            object_index: 0,
                            offset: 0,
                            pitch: 0,
                        },
                        AvDrmPlaneDescriptor {
                            object_index: 0,
                            offset: 0,
                            pitch: 0,
                        },
                    ],
                },
                AvDrmLayerDescriptor {
                    format: 0,
                    nb_planes: 0,
                    planes: [AvDrmPlaneDescriptor {
                        object_index: 0,
                        offset: 0,
                        pitch: 0,
                    }; 4],
                },
                AvDrmLayerDescriptor {
                    format: 0,
                    nb_planes: 0,
                    planes: [AvDrmPlaneDescriptor {
                        object_index: 0,
                        offset: 0,
                        pitch: 0,
                    }; 4],
                },
                AvDrmLayerDescriptor {
                    format: 0,
                    nb_planes: 0,
                    planes: [AvDrmPlaneDescriptor {
                        object_index: 0,
                        offset: 0,
                        pitch: 0,
                    }; 4],
                },
            ],
        };

        let parsed = drm_prime_descriptor_from_raw(&desc, 4).expect("parse");
        assert_eq!(parsed.format, FourCc::new(*b"NV12"));
        assert_eq!(parsed.planes.len(), 2);
        assert_eq!(parsed.planes[0].len, 2560);
        assert_eq!(parsed.planes[1].offset, 2048);
        assert_eq!(parsed.planes[1].len, 1280);
        assert_eq!(parsed.backing_bytes, 4096);
    }

    #[test]
    fn prefer_drm_prime_format_picks_drm_when_offered() {
        let formats = [
            ffi::AVPixelFormat::AV_PIX_FMT_NV12,
            ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME,
            ffi::AVPixelFormat::AV_PIX_FMT_NONE,
        ];
        let picked = unsafe { prefer_drm_prime_format(ptr::null_mut(), formats.as_ptr()) };
        assert_eq!(picked, ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME);
    }

    #[test]
    fn prefer_drm_prime_format_falls_back_to_first() {
        let formats = [
            ffi::AVPixelFormat::AV_PIX_FMT_NV12,
            ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
            ffi::AVPixelFormat::AV_PIX_FMT_NONE,
        ];
        let picked = unsafe { prefer_drm_prime_format(ptr::null_mut(), formats.as_ptr()) };
        assert_eq!(picked, ffi::AVPixelFormat::AV_PIX_FMT_NV12);
    }
}

#[path = "decoder_codecs.rs"]
mod decoder_codecs;
pub use decoder_codecs::{FfmpegH264Decoder, FfmpegH265Decoder, FfmpegMjpegDecoder};
