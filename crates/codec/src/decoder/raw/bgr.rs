use styx_core::prelude::*;

use crate::decoder::raw::decode_strided_rows_to_rgb24;
#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError};

/// BGR24 → RGB24 decoder (channel swap).
pub struct BgrToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl BgrToRgbDecoder {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_input(BufferPool::lazy(bytes, 4), FourCc::BGR3, "bgr-swap")
    }

    pub fn with_pool(pool: BufferPool) -> Self {
        Self::with_input(pool, FourCc::BGR3, "bgr-swap")
    }

    pub fn with_input(pool: BufferPool, input: FourCc, impl_name: &'static str) -> Self {
        Self {
            descriptor: crate::decoder::raw::raw_decoder_descriptor(
                input,
                FourCc::RG24,
                "bgr2rgb",
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
            .ok_or_else(|| CodecError::Codec("bgr frame missing plane".into()))?;

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let stride = plane.stride().max(width * 3);
        let required = stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("bgr stride overflow".into()))?;
        if plane.data().len() < required {
            return Err(CodecError::Codec("bgr plane buffer too short".into()));
        }

        let row_bytes = width * 3;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("bgr output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("bgr dst buffer too short".into()));
        }

        let src = plane.data();
        decode_strided_rows_to_rgb24(
            src,
            &mut dst[..out_len],
            height,
            stride,
            width * 3,
            row_bytes,
            |src_line, dst_line| {
                styx_core::simd::swap_rb24_row(src_line, dst_line, width);
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

impl Codec for BgrToRgbDecoder {
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

#[cfg(test)]
// This module still keeps compact inline tests next to decoder helpers.
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn decode_into_matches_process() {
        let res = Resolution::new(2, 1).unwrap();
        let format = MediaFormat::new(FourCc::BGR3, res, ColorSpace::Srgb);
        let src = [1u8, 2, 3, 4, 5, 6]; // BGR BGR
        let make_frame = || {
            let mut buf = BufferPool::with_limits(1, 6, 1).lease();
            buf.resize(6);
            buf.as_mut_slice().copy_from_slice(&src);
            FrameLease::single_plane(FrameMeta::new(format, 123), buf, 6, 6)
        };

        let dec = BgrToRgbDecoder::with_pool(BufferPool::with_limits(1, 6, 1));
        let processed = dec.process(make_frame()).unwrap();
        let plane = processed.planes().into_iter().next().unwrap();

        let mut out = vec![0u8; 6];
        let frame = make_frame();
        dec.decode_into(&frame, &mut out).unwrap();

        assert_eq!(plane.data(), out.as_slice());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_shared_outputs_exportable_memfd_frame() {
        use crate::decoder::raw::SharedRawDecodeExt;

        let res = Resolution::new(2, 1).unwrap();
        let format = MediaFormat::new(FourCc::BGR3, res, ColorSpace::Srgb);
        let mut buf = BufferPool::with_limits(1, 6, 1).lease();
        buf.resize(6);
        buf.as_mut_slice().copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        let frame = FrameLease::single_plane(FrameMeta::new(format, 123), buf, 6, 6);

        let dec = BgrToRgbDecoder::with_pool(BufferPool::with_limits(1, 6, 1));
        let pool = SharedBufferPool::with_capacity(1, 6).unwrap();
        let out = SharedRawDecodeExt::process_shared(&dec, &frame, &pool).unwrap();
        assert_eq!(out.external_backing_kind(), Some("memfd_pool"));
        assert_eq!(out.planes()[0].data(), &[3, 2, 1, 6, 5, 4]);

        let (descriptor, backing) = out.export_or_copy_memfd().unwrap();
        let FrameBackingExport::Memfd { fd, len } = backing else {
            panic!("shared raw decode should export as memfd");
        };
        assert_eq!(len, 6);
        let imported = FrameLease::from_memfd_import(descriptor, fd).unwrap();
        assert_eq!(imported.planes()[0].data(), &[3, 2, 1, 6, 5, 4]);
    }
}

#[cfg(feature = "image")]
impl ImageDecode for BgrToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}
