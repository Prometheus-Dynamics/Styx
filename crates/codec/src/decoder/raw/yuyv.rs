use styx_core::prelude::*;

use crate::decoder::raw::yuv_to_rgb;
#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError, CodecKind};
use yuvutils_rs::{YuvPackedImage, YuvRange, YuvStandardMatrix};

#[inline]
fn normalized_packed_stride(plane: &Plane<'_>, width: usize, height: usize, bytes_per_pixel: usize) -> usize {
    let min_stride = width.saturating_mul(bytes_per_pixel);
    let mut stride = plane.stride().max(min_stride);
    if let Some(tight_len) = min_stride.checked_mul(height) {
        if plane.data().len() == tight_len {
            stride = min_stride;
        }
    }
    stride
}

#[inline(always)]
fn map_colorspace(color: ColorSpace) -> (YuvRange, YuvStandardMatrix) {
    match color {
        ColorSpace::Bt709 => (YuvRange::Limited, YuvStandardMatrix::Bt709),
        ColorSpace::Bt2020 => (YuvRange::Limited, YuvStandardMatrix::Bt2020),
        // In our frame metadata `Srgb` is used as a "full-range output" hint; libcamera often
        // reports `sYCC` for PiSP outputs, which uses a Rec.601 YCbCr matrix.
        ColorSpace::Srgb => (YuvRange::Full, YuvStandardMatrix::Bt601),
        ColorSpace::Unknown => (YuvRange::Limited, YuvStandardMatrix::Bt709),
    }
}

/// CPU YUYV422 → RGB24 decoder.
pub struct YuyvToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl YuyvToRgbDecoder {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_pool(BufferPool::with_limits(2, bytes, 4))
    }

    pub fn with_pool(pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: FourCc::new(*b"YUYV"),
                output: FourCc::new(*b"RG24"),
                name: "yuv2rgb",
                impl_name: "yuyv-cpu",
            },
            pool,
        }
    }

    /// Decode into a caller-provided tightly-packed RGB24 buffer.
    ///
    /// `dst` must be at least `width * height * 3` bytes.
    pub fn decode_into(&self, input: &FrameLease, dst: &mut [u8]) -> Result<FrameMeta, CodecError> {
        let meta = input.meta();
        if meta.format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: meta.format.code,
            });
        }
        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let color = meta.format.color;
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("yuyv frame missing plane".into()))?;
        let stride = normalized_packed_stride(&plane, width, height, 2);
        let required = stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("yuyv stride overflow".into()))?;
        if plane.data().len() < required {
            return Err(CodecError::Codec("yuyv plane buffer too short".into()));
        }

        let row_bytes = width
            .checked_mul(3)
            .ok_or_else(|| CodecError::Codec("yuyv output overflow".into()))?;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("yuyv output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("yuyv dst buffer too short".into()));
        }
        let dst = &mut dst[..out_len];

        let src = &plane.data()[..required];
        let packed = YuvPackedImage {
            yuy: src,
            yuy_stride: stride as u32,
            width: width as u32,
            height: height as u32,
        };
        let (range, matrix) = map_colorspace(color);
        if yuvutils_rs::yuyv422_to_rgb(&packed, dst, row_bytes as u32, range, matrix).is_err() {
            for y in 0..height {
                let src_line = &src[y * stride..][..width * 2];
                let dst_line = &mut dst[y * row_bytes..(y + 1) * row_bytes];
                for (dst_px, src_px) in dst_line
                    .chunks_exact_mut(6)
                    .zip(src_line[..width * 2].chunks_exact(4))
                {
                    let y0 = unsafe { *src_px.get_unchecked(0) as i32 };
                    let u = unsafe { *src_px.get_unchecked(1) as i32 };
                    let y1 = unsafe { *src_px.get_unchecked(2) as i32 };
                    let v = unsafe { *src_px.get_unchecked(3) as i32 };
                    let (r0, g0, b0) = yuv_to_rgb(y0, u, v, color);
                    let (r1, g1, b1) = yuv_to_rgb(y1, u, v, color);
                    unsafe {
                        *dst_px.get_unchecked_mut(0) = r0;
                        *dst_px.get_unchecked_mut(1) = g0;
                        *dst_px.get_unchecked_mut(2) = b0;
                        *dst_px.get_unchecked_mut(3) = r1;
                        *dst_px.get_unchecked_mut(4) = g1;
                        *dst_px.get_unchecked_mut(5) = b1;
                    }
                }
            }
        }

        Ok(FrameMeta::new(
            MediaFormat::new(
                self.descriptor.output,
                meta.format.resolution,
                meta.format.color,
            ),
            meta.timestamp,
        ))
    }
}

impl Codec for YuyvToRgbDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        let layout = plane_layout_from_dims(
            input.meta().format.resolution.width,
            input.meta().format.resolution.height,
            3,
        );
        let mut buf = self.pool.lease();
        unsafe { buf.resize_uninit(layout.len) };
        let meta = self.decode_into(&input, buf.as_mut_slice())?;
        Ok(unsafe { FrameLease::single_plane_uninit(meta, buf, layout.len, layout.stride) })
    }
}

#[cfg(feature = "image")]
impl ImageDecode for YuyvToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

/// CPU YUYV422 → GREY decoder (extract luma plane).
pub struct YuyvToLumaDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl YuyvToLumaDecoder {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize;
        Self::with_pool(BufferPool::with_limits(2, bytes, 4))
    }

    pub fn with_pool(pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: FourCc::new(*b"YUYV"),
                output: FourCc::new(*b"GREY"),
                name: "yuv2luma",
                impl_name: "yuyv-luma",
            },
            pool,
        }
    }

    /// Decode into a caller-provided tightly-packed GREY buffer.
    ///
    /// `dst` must be at least `width * height` bytes.
    pub fn decode_into(&self, input: &FrameLease, dst: &mut [u8]) -> Result<FrameMeta, CodecError> {
        let meta = input.meta();
        if meta.format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: meta.format.code,
            });
        }
        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("yuyv frame missing plane".into()))?;
        let stride = normalized_packed_stride(&plane, width, height, 2);
        let required = stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("yuyv stride overflow".into()))?;
        if plane.data().len() < required {
            return Err(CodecError::Codec("yuyv plane buffer too short".into()));
        }

        let out_len = width
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("yuyv luma output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("yuyv luma dst buffer too short".into()));
        }
        let dst = &mut dst[..out_len];
        let src = &plane.data()[..required];

        #[cfg(target_arch = "aarch64")]
        {
            use std::arch::aarch64::{vld2q_u8, vst1q_u8};
            for y in 0..height {
                let src_line = &src[y * stride..][..width * 2];
                let dst_line = &mut dst[y * width..(y + 1) * width];
                unsafe {
                    let src_ptr = src_line.as_ptr();
                    let dst_ptr = dst_line.as_mut_ptr();
                    let blocks = width / 16;
                    for i in 0..blocks {
                        let src_block = src_ptr.add(i * 32) as *const u8;
                        let yuv = vld2q_u8(src_block);
                        vst1q_u8(dst_ptr.add(i * 16), yuv.0);
                    }
                    let start = blocks * 16;
                    for x in start..width {
                        *dst_ptr.add(x) = *src_ptr.add(x * 2);
                    }
                }
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            for y in 0..height {
                let src_line = &src[y * stride..][..width * 2];
                let dst_line = &mut dst[y * width..(y + 1) * width];
                unsafe {
                    let src_ptr = src_line.as_ptr();
                    let dst_ptr = dst_line.as_mut_ptr();
                    for x in 0..width {
                        *dst_ptr.add(x) = *src_ptr.add(x * 2);
                    }
                }
            }
        }

        Ok(FrameMeta::new(
            MediaFormat::new(
                self.descriptor.output,
                meta.format.resolution,
                meta.format.color,
            ),
            meta.timestamp,
        ))
    }
}

impl Codec for YuyvToLumaDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        let layout = plane_layout_from_dims(
            input.meta().format.resolution.width,
            input.meta().format.resolution.height,
            1,
        );
        let mut buf = self.pool.lease();
        unsafe { buf.resize_uninit(layout.len) };
        let meta = self.decode_into(&input, buf.as_mut_slice())?;
        Ok(unsafe { FrameLease::single_plane_uninit(meta, buf, layout.len, layout.stride) })
    }
}

#[cfg(feature = "image")]
impl ImageDecode for YuyvToLumaDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_yuyv_frame(bytes: &[u8], res: Resolution, stride: usize) -> FrameLease {
        let mut buf = BufferPool::with_limits(1, bytes.len(), 1).lease();
        buf.resize(bytes.len());
        buf.as_mut_slice().copy_from_slice(bytes);
        FrameLease::single_plane(
            FrameMeta::new(MediaFormat::new(FourCc::new(*b"YUYV"), res, ColorSpace::Srgb), 1),
            buf,
            bytes.len(),
            stride,
        )
    }

    #[test]
    fn yuyv_decode_into_matches_process_rgb() {
        let res = Resolution::new(2, 1).unwrap();
        let bytes = [10u8, 128, 20, 128];
        let make = || make_yuyv_frame(&bytes, res, 4);
        let dec = YuyvToRgbDecoder::new(2, 1);

        let processed = dec.process(make()).unwrap();
        let plane = processed.planes().into_iter().next().unwrap();

        let mut out = vec![0u8; 2 * 3];
        let frame = make();
        dec.decode_into(&frame, &mut out).unwrap();
        assert_eq!(plane.data(), out.as_slice());
    }

    #[test]
    fn yuyv_decode_into_matches_process_luma() {
        let res = Resolution::new(2, 1).unwrap();
        let bytes = [10u8, 128, 20, 128];
        let make = || make_yuyv_frame(&bytes, res, 4);
        let dec = YuyvToLumaDecoder::new(2, 1);

        let processed = dec.process(make()).unwrap();
        let plane = processed.planes().into_iter().next().unwrap();

        let mut out = vec![0u8; 2];
        let frame = make();
        dec.decode_into(&frame, &mut out).unwrap();
        assert_eq!(plane.data(), out.as_slice());
    }
}
