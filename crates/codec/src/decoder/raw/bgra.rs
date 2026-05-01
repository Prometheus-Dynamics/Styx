use styx_core::prelude::*;

use crate::decoder::raw::decode_strided_rows_to_rgb24;
#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError};

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn bgra_row_to_rgb24_neon(src: &[u8], dst: &mut [u8], width: usize) {
    use std::arch::aarch64::{uint8x16x3_t, vld4q_u8, vst3q_u8};
    debug_assert!(src.len() >= width * 4);
    debug_assert!(dst.len() >= width * 3);

    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();

    let mut x = 0usize;
    while x + 16 <= width {
        unsafe {
            let bgra = vld4q_u8(src_ptr.add(x * 4));
            let rgb = uint8x16x3_t(bgra.2, bgra.1, bgra.0);
            vst3q_u8(dst_ptr.add(x * 3), rgb);
        }
        x += 16;
    }
    for x in x..width {
        unsafe {
            let si = x * 4;
            let di = x * 3;
            let b = *src_ptr.add(si);
            let g = *src_ptr.add(si + 1);
            let r = *src_ptr.add(si + 2);
            *dst_ptr.add(di) = r;
            *dst_ptr.add(di + 1) = g;
            *dst_ptr.add(di + 2) = b;
        }
    }
}

/// BGRA → RGB24 decoder (drops alpha and reorders channels).
pub struct BgraToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl BgraToRgbDecoder {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_input(BufferPool::lazy(bytes, 4), FourCc::BGRA, "bgra-strip")
    }

    pub fn with_pool(pool: BufferPool) -> Self {
        Self::with_input(pool, FourCc::BGRA, "bgra-strip")
    }

    pub fn with_input(pool: BufferPool, input: FourCc, impl_name: &'static str) -> Self {
        Self {
            descriptor: crate::decoder::raw::raw_decoder_descriptor(
                input,
                FourCc::RG24,
                "bgra2rgb",
                impl_name,
            ),
            pool,
        }
    }

    pub fn with_input_for_max(
        input: FourCc,
        impl_name: &'static str,
        max_width: u32,
        max_height: u32,
    ) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_input(BufferPool::lazy(bytes, 4), input, impl_name)
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
            .ok_or_else(|| CodecError::Codec("bgra frame missing plane".into()))?;

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let stride = plane.stride().max(width * 4);
        let required = stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("bgra stride overflow".into()))?;
        if plane.data().len() < required {
            return Err(CodecError::Codec("bgra plane buffer too short".into()));
        }

        let row_bytes = width * 3;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("bgra output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("bgra dst buffer too short".into()));
        }

        let src = plane.data();
        decode_strided_rows_to_rgb24(
            src,
            &mut dst[..out_len],
            height,
            stride,
            width * 4,
            row_bytes,
            |src_line, dst_line| {
                #[cfg(target_arch = "aarch64")]
                unsafe {
                    bgra_row_to_rgb24_neon(src_line, dst_line, width);
                    return;
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    for (dst_px, src_px) in
                        dst_line.chunks_exact_mut(3).zip(src_line.chunks_exact(4))
                    {
                        dst_px[0] = src_px[2];
                        dst_px[1] = src_px[1];
                        dst_px[2] = src_px[0];
                    }
                }
            },
        );

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

impl Codec for BgraToRgbDecoder {
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
impl ImageDecode for BgraToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}
