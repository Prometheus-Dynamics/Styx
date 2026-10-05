use styx_core::prelude::*;

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError};

/// 16-bit per channel RGB/BGR → RGB24 (drop precision, optional swap).
pub struct Rgb48ToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    swap_rb: bool,
}

impl Rgb48ToRgbDecoder {
    pub fn new(
        input: FourCc,
        impl_name: &'static str,
        swap_rb: bool,
        max_width: u32,
        max_height: u32,
    ) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self {
            descriptor: crate::decoder::raw::raw_decoder_descriptor(
                input,
                FourCc::RG24,
                "rgb48torgb24",
                impl_name,
            ),
            pool: BufferPool::lazy(bytes, 4),
            swap_rb,
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
            .ok_or_else(|| CodecError::Codec("rgb48 frame missing plane".into()))?;
        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let stride = plane.stride().max(width * 6);
        let required = stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("rgb48 stride overflow".into()))?;
        if plane.data().len() < required {
            return Err(CodecError::Codec("rgb48 plane buffer too short".into()));
        }

        let row_bytes = width
            .checked_mul(3)
            .ok_or_else(|| CodecError::Codec("rgb48 output overflow".into()))?;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("rgb48 output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("rgb48 dst buffer too short".into()));
        }

        let data = plane.data();
        let swap_rb = self.swap_rb;
        crate::par::for_each_row(&mut dst[..out_len], row_bytes, |y, dst_line| {
            let src_line = &data[y * stride..][..width * 6];
            styx_core::simd::rgb48le_to_rgb24_row(src_line, dst_line, width, swap_rb);
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

impl Codec for Rgb48ToRgbDecoder {
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
impl ImageDecode for Rgb48ToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}
