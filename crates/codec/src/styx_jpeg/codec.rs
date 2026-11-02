//! [`StyxJpegEncoder`]: the encoder as a [`Codec`] (frames in, MJPG out).

use std::sync::Mutex;

use styx_core::prelude::*;
use styx_core::simd::ColorLayout;

use super::{StyxJpeg, StyxJpegInput};
#[cfg(target_os = "linux")]
use crate::shared_packet_frame;
use crate::{
    Codec, CodecDescriptor, CodecError, CodecKind, DEFAULT_CODEC_POOL_CHUNK_BYTES,
    DEFAULT_CODEC_POOL_SPARE,
};

/// MJPEG from RG24, BG24, RGBA, BGRA, I420 (YU12), NV12 or GREY (R8) frames with Styx's own
/// encoder ([`StyxJpeg`]).
pub struct StyxJpegEncoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    /// The encoder and its output, kept between frames.
    state: Mutex<(StyxJpeg, Vec<u8>)>,
}

impl StyxJpegEncoder {
    /// The input formats taken.
    pub const INPUTS: [FourCc; 9] = [
        FourCc::RG24,
        FourCc::BG24,
        FourCc::RGBA,
        FourCc::BGRA,
        FourCc::I420,
        FourCc::YU12,
        FourCc::NV12,
        FourCc::GREY,
        FourCc::R8,
    ];

    pub fn new(input: FourCc, quality: u8) -> Self {
        Self::with_pool(
            input,
            quality,
            BufferPool::lazy(DEFAULT_CODEC_POOL_CHUNK_BYTES, DEFAULT_CODEC_POOL_SPARE),
        )
    }

    pub fn with_pool(input: FourCc, quality: u8, pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Encoder,
                input,
                output: FourCc::MJPG,
                name: "mjpeg",
                impl_name: "styx-jpeg",
            },
            pool,
            state: Mutex::new((StyxJpeg::new(quality), Vec::new())),
        }
    }

    /// Encode `input` and hand the JPEG bytes to `emit`.
    fn encode<T>(
        &self,
        input: &FrameLease,
        emit: impl FnOnce(&FrameMeta, &[u8]) -> Result<T, CodecError>,
    ) -> Result<T, CodecError> {
        let meta = input.meta();
        if meta.format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: meta.format.code,
            });
        }
        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let planes = input.planes();
        let plane = |i: usize| {
            planes
                .get(i)
                .ok_or_else(|| CodecError::Codec("jpeg encoder frame missing plane".into()))
        };
        let first = plane(0)?;
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let packed = |layout: ColorLayout| StyxJpegInput::Packed {
            data: first.data(),
            width,
            height,
            stride: first.stride().max(width * layout.bytes_per_pixel()),
            layout,
        };
        let pixels = match self.descriptor.input {
            FourCc::RG24 => packed(ColorLayout::Rgb24),
            FourCc::BG24 => packed(ColorLayout::Bgr24),
            FourCc::RGBA => packed(ColorLayout::Rgba32),
            FourCc::BGRA => packed(ColorLayout::Bgra32),
            FourCc::GREY | FourCc::R8 => StyxJpegInput::Gray {
                y: first.data(),
                width,
                height,
                stride: first.stride().max(width),
            },
            FourCc::I420 | FourCc::YU12 => {
                let y_stride = first.stride().max(width);
                if planes.len() >= 3 {
                    StyxJpegInput::I420 {
                        y: first.data(),
                        u: plane(1)?.data(),
                        v: plane(2)?.data(),
                        width,
                        height,
                        y_stride,
                        c_stride: plane(1)?.stride().max(cw),
                    }
                } else {
                    // One plane: Y, then U and V at half the stride.
                    let data = first.data();
                    let c_stride = y_stride.div_ceil(2);
                    let (y, rest) = data.split_at((y_stride * height).min(data.len()));
                    let (u, v) = rest.split_at((c_stride * ch).min(rest.len()));
                    StyxJpegInput::I420 {
                        y,
                        u,
                        v,
                        width,
                        height,
                        y_stride,
                        c_stride,
                    }
                }
            }
            FourCc::NV12 => {
                if planes.len() >= 2 {
                    StyxJpegInput::Nv12 {
                        y: first.data(),
                        uv: plane(1)?.data(),
                        width,
                        height,
                        y_stride: first.stride().max(width),
                        uv_stride: plane(1)?.stride().max(2 * cw),
                    }
                } else {
                    // One plane: the chroma rows of an odd width hold one more byte.
                    let stride = first.stride().max(width).max(2 * cw);
                    let data = first.data();
                    let (y, uv) = data.split_at((stride * height).min(data.len()));
                    StyxJpegInput::Nv12 {
                        y,
                        uv,
                        width,
                        height,
                        y_stride: stride,
                        uv_stride: stride,
                    }
                }
            }
            other => {
                return Err(CodecError::Codec(format!(
                    "styx-jpeg does not encode {other}"
                )));
            }
        };
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (encoder, jpeg) = &mut *guard;
        encoder.encode(&pixels, jpeg)?;
        emit(meta, jpeg)
    }
}

impl Codec for StyxJpegEncoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn memory_stats(&self) -> Option<BufferPoolStats> {
        Some(self.pool.stats())
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        self.process_ref(&input)
    }

    fn process_ref(&self, input: &FrameLease) -> Result<FrameLease, CodecError> {
        self.encode(input, |meta, jpeg| {
            let mut buf = self.pool.lease();
            buf.resize(jpeg.len());
            buf.as_mut_slice()[..jpeg.len()].copy_from_slice(jpeg);
            Ok(FrameLease::single_plane(
                FrameMeta::new(
                    MediaFormat::new(
                        self.descriptor.output,
                        meta.format.resolution,
                        meta.format.color,
                    ),
                    meta.timestamp,
                ),
                buf,
                jpeg.len(),
                jpeg.len(),
            ))
        })
    }

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        self.encode(input, |meta, jpeg| {
            shared_packet_frame(&self.descriptor, meta, jpeg, pool).map(Some)
        })
    }
}
