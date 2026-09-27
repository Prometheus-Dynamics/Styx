use styx_core::prelude::*;

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError};
use rayon::prelude::*;

/// Monochrome 8-bit → RGB24 (channel replicate).
pub struct Mono8ToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl Mono8ToRgbDecoder {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_pool(BufferPool::lazy(bytes, 4))
    }

    pub fn with_pool(pool: BufferPool) -> Self {
        Self {
            descriptor: crate::decoder::raw::raw_decoder_descriptor(
                FourCc::R8,
                FourCc::RG24,
                "mono2rgb",
                "mono8-replicate",
            ),
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
                styx_core::simd::gray8_to_rgb24_row(src_line, dst_line, width);
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
        crate::decoder::raw::process_owned_raw_decode(input, &self.pool, 3, |input, dst| {
            self.decode_into(input, dst)
        })
    }

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        crate::decoder::raw::process_shared_raw_decode(self, input, pool)
    }
}

#[cfg(feature = "image")]
impl ImageDecode for Mono8ToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

#[cfg(test)]
// This module still keeps compact inline tests next to decoder helpers.
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
            FrameMeta::new(MediaFormat::new(FourCc::R8, res, ColorSpace::Unknown), 0),
            buf,
            stride * height,
            stride,
        );
        let out = decoder.process(frame).unwrap();
        assert_eq!(out.meta().format.code, FourCc::RG24);
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
            FrameMeta::new(MediaFormat::new(FourCc::R16, res, ColorSpace::Unknown), 0),
            buf,
            src.len(),
            stride,
        );
        let out = decoder.process(frame).unwrap();
        assert_eq!(out.meta().format.code, FourCc::RG24);
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
        Self::with_pool(BufferPool::lazy(bytes, 4))
    }

    pub fn with_pool(pool: BufferPool) -> Self {
        Self {
            descriptor: crate::decoder::raw::raw_decoder_descriptor(
                FourCc::R16,
                FourCc::RG24,
                "mono2rgb",
                "mono16-replicate",
            ),
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
                styx_core::simd::gray16le_to_rgb24_row(src_line, dst_line, width);
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
        crate::decoder::raw::process_owned_raw_decode(input, &self.pool, 3, |input, dst| {
            self.decode_into(input, dst)
        })
    }

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        crate::decoder::raw::process_shared_raw_decode(self, input, pool)
    }
}

#[cfg(feature = "image")]
impl ImageDecode for Mono16ToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}
