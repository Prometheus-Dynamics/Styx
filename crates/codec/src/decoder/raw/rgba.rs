use styx_core::prelude::*;

use crate::decoder::raw::decode_strided_rows_to_rgb24;
#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError};

/// RGBA → RGB24 decoder (drops alpha).
pub struct RgbaToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl RgbaToRgbDecoder {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_input(BufferPool::lazy(bytes, 4), FourCc::RGBA, "rgba-strip")
    }

    pub fn with_pool(pool: BufferPool) -> Self {
        Self::with_input(pool, FourCc::RGBA, "rgba-strip")
    }

    pub fn with_input(pool: BufferPool, input: FourCc, impl_name: &'static str) -> Self {
        Self {
            descriptor: crate::decoder::raw::raw_decoder_descriptor(
                input,
                FourCc::RG24,
                "rgba2rgb",
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
            .ok_or_else(|| CodecError::Codec("rgba frame missing plane".into()))?;

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let stride = plane.stride().max(width * 4);
        let required = stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("rgba stride overflow".into()))?;
        if plane.data().len() < required {
            return Err(CodecError::Codec("rgba plane buffer too short".into()));
        }

        let row_bytes = width * 3;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("rgba output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("rgba dst buffer too short".into()));
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
                styx_core::simd::x32_to_rgb24_row(src_line, dst_line, width, false);
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

impl Codec for RgbaToRgbDecoder {
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
impl ImageDecode for RgbaToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}
