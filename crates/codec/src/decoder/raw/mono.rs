use styx_core::prelude::*;

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError, CodecKind};
use rayon::prelude::*;

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn mono8_row_to_rgb24_neon(src: &[u8], dst: &mut [u8], width: usize) {
    use std::arch::aarch64::{uint8x16x3_t, vld1q_u8, vst3q_u8};
    debug_assert!(src.len() >= width);
    debug_assert!(dst.len() >= width * 3);

    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let mut x = 0usize;
    while x + 16 <= width {
        unsafe {
            let g = vld1q_u8(src_ptr.add(x));
            let rgb = uint8x16x3_t(g, g, g);
            vst3q_u8(dst_ptr.add(x * 3), rgb);
        }
        x += 16;
    }
    for x in x..width {
        unsafe {
            let gray = *src_ptr.add(x);
            let o = x * 3;
            *dst_ptr.add(o) = gray;
            *dst_ptr.add(o + 1) = gray;
            *dst_ptr.add(o + 2) = gray;
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn mono16_row_to_rgb24_neon(src: &[u8], dst: &mut [u8], width: usize) -> bool {
    use std::arch::aarch64::{uint8x8x3_t, vld1q_u16, vshrn_n_u16, vst3_u8};
    debug_assert!(src.len() >= width * 2);
    debug_assert!(dst.len() >= width * 3);

    // NEON loads u16 lanes; require row pointer to be 2-byte aligned.
    if (src.as_ptr() as usize) & 0x1 != 0 {
        return false;
    }

    let src_u16 = src.as_ptr() as *const u16;
    let dst_ptr = dst.as_mut_ptr();
    let mut x = 0usize;
    while x + 8 <= width {
        unsafe {
            let v16 = vld1q_u16(src_u16.add(x));
            let g8 = vshrn_n_u16(v16, 8);
            vst3_u8(dst_ptr.add(x * 3), uint8x8x3_t(g8, g8, g8));
        }
        x += 8;
    }
    for x in x..width {
        unsafe {
            let si = x * 2;
            let v = u16::from_le_bytes([*src.get_unchecked(si), *src.get_unchecked(si + 1)]);
            let g = (v >> 8) as u8;
            let di = x * 3;
            *dst_ptr.add(di) = g;
            *dst_ptr.add(di + 1) = g;
            *dst_ptr.add(di + 2) = g;
        }
    }
    true
}

/// Monochrome 8-bit → RGB24 (channel replicate).
pub struct Mono8ToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl Mono8ToRgbDecoder {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_pool(BufferPool::with_limits(2, bytes, 4))
    }

    pub fn with_pool(pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: FourCc::new(*b"R8  "),
                output: FourCc::new(*b"RG24"),
                name: "mono2rgb",
                impl_name: "mono8-replicate",
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
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("mono frame missing plane".into()))?;

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let stride = plane.stride().max(width);
        let required = stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("mono stride overflow".into()))?;
        if plane.data().len() < required {
            return Err(CodecError::Codec("mono plane buffer too short".into()));
        }

        let row_bytes = width * 3;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("mono output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("mono dst buffer too short".into()));
        }

        let src = plane.data();
        dst[..out_len]
            .par_chunks_mut(row_bytes)
            .enumerate()
            .for_each(|(y, dst_line)| {
                let src_line = &src[y * stride..][..width];
                #[cfg(target_arch = "aarch64")]
                unsafe {
                    mono8_row_to_rgb24_neon(src_line, dst_line, width);
                    return;
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    for (dst_px, &gray) in dst_line.chunks_exact_mut(3).zip(src_line.iter()) {
                        dst_px[0] = gray;
                        dst_px[1] = gray;
                        dst_px[2] = gray;
                    }
                }
            });

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

impl Codec for Mono8ToRgbDecoder {
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

        Ok(unsafe {
            FrameLease::single_plane_uninit(
                meta,
                buf,
                layout.len,
                layout.stride,
            )
        })
    }
}

#[cfg(feature = "image")]
impl ImageDecode for Mono8ToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn mono8_expands_to_rgb24() {
        let decoder = Mono8ToRgbDecoder::new(16, 4);
        let res = Resolution::new(5, 3).unwrap();
        let height = res.height.get() as usize;
        let width = res.width.get() as usize;
        let stride = 8usize;
        let mut src = vec![0u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                src[y * stride + x] = (y as u8) * 10 + (x as u8);
            }
        }

        let pool = BufferPool::with_limits(1, stride * height, 4);
        let mut buf = pool.lease();
        buf.resize(src.len());
        buf.as_mut_slice().copy_from_slice(&src);

        let frame = FrameLease::single_plane(
            FrameMeta::new(
                MediaFormat::new(FourCc::new(*b"R8  "), res, ColorSpace::Unknown),
                0,
            ),
            buf,
            stride * height,
            stride,
        );
        let out = decoder.process(frame).unwrap();
        assert_eq!(out.meta().format.code, FourCc::new(*b"RG24"));
        let planes = out.planes();
        assert_eq!(planes.len(), 1);
        let data = planes[0].data();
        assert_eq!(data.len(), width * height * 3);
        for y in 0..height {
            for x in 0..width {
                let g = (y as u8) * 10 + (x as u8);
                let o = (y * width + x) * 3;
                assert_eq!(&data[o..o + 3], &[g, g, g]);
            }
        }
    }

    #[test]
    fn mono16_expands_to_rgb24() {
        let decoder = Mono16ToRgbDecoder::new(16, 4);
        let res = Resolution::new(5, 3).unwrap();
        let height = res.height.get() as usize;
        let width = res.width.get() as usize;
        let stride = 12usize;

        let mut src = vec![0u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                let v = (((y * width + x) * 257) as u16).to_le_bytes();
                let o = y * stride + x * 2;
                src[o] = v[0];
                src[o + 1] = v[1];
            }
        }

        let pool = BufferPool::with_limits(1, src.len(), 4);
        let mut buf = pool.lease();
        buf.resize(src.len());
        buf.as_mut_slice().copy_from_slice(&src);

        let frame = FrameLease::single_plane(
            FrameMeta::new(
                MediaFormat::new(FourCc::new(*b"R16 "), res, ColorSpace::Unknown),
                0,
            ),
            buf,
            src.len(),
            stride,
        );
        let out = decoder.process(frame).unwrap();
        assert_eq!(out.meta().format.code, FourCc::new(*b"RG24"));
        let planes = out.planes();
        assert_eq!(planes.len(), 1);
        let data = planes[0].data();
        assert_eq!(data.len(), width * height * 3);
        for y in 0..height {
            for x in 0..width {
                let v = ((y * width + x) * 257) as u16;
                let g = (v >> 8) as u8;
                let o = (y * width + x) * 3;
                assert_eq!(&data[o..o + 3], &[g, g, g]);
            }
        }
    }
}

/// Monochrome 16-bit little-endian → RGB24 (downshift + replicate).
pub struct Mono16ToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl Mono16ToRgbDecoder {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_pool(BufferPool::with_limits(2, bytes, 4))
    }

    pub fn with_pool(pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: FourCc::new(*b"R16 "),
                output: FourCc::new(*b"RG24"),
                name: "mono2rgb",
                impl_name: "mono16-replicate",
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
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("mono16 frame missing plane".into()))?;

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let stride = plane.stride().max(width * 2);
        let required = stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("mono16 stride overflow".into()))?;
        if plane.data().len() < required {
            return Err(CodecError::Codec("mono16 plane buffer too short".into()));
        }

        let row_bytes = width * 3;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("mono16 output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("mono16 dst buffer too short".into()));
        }

        let src = plane.data();
        dst[..out_len]
            .par_chunks_mut(row_bytes)
            .enumerate()
            .for_each(|(y, dst_line)| {
                let src_line = &src[y * stride..][..width * 2];
                #[cfg(target_arch = "aarch64")]
                unsafe {
                    if mono16_row_to_rgb24_neon(src_line, dst_line, width) {
                        return;
                    }
                }
                for (dst_px, chunk) in dst_line.chunks_exact_mut(3).zip(src_line.chunks_exact(2)) {
                    let gray16 = u16::from_le_bytes([chunk[0], chunk[1]]);
                    let gray8 = (gray16 >> 8) as u8;
                    dst_px[0] = gray8;
                    dst_px[1] = gray8;
                    dst_px[2] = gray8;
                }
            });

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

impl Codec for Mono16ToRgbDecoder {
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

        Ok(unsafe {
            FrameLease::single_plane_uninit(
                meta,
                buf,
                layout.len,
                layout.stride,
            )
        })
    }
}

#[cfg(feature = "image")]
impl ImageDecode for Mono16ToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}
