use styx_core::prelude::*;

use crate::decoder::raw::STYX_YUV_ROWS;
#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError};
use yuvutils_rs::YuvPlanarImage;

/// CPU I420 (YUV420 planar) → RGB24 decoder.
pub struct I420ToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl I420ToRgbDecoder {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_pool(BufferPool::lazy(bytes, 4))
    }

    pub fn with_pool(pool: BufferPool) -> Self {
        Self {
            descriptor: crate::decoder::raw::raw_decoder_descriptor(
                FourCc::I420,
                FourCc::RG24,
                "yuv2rgb",
                "i420-cpu",
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
        let planes = input.planes();
        if planes.len() < 3 {
            return Err(CodecError::Codec("i420 frame missing planes".into()));
        }
        let y_plane = &planes[0];
        let u_plane = &planes[1];
        let v_plane = &planes[2];

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let color = meta.format.color;
        let y_stride = y_plane.stride().max(width);
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);
        let u_stride = u_plane.stride().max(chroma_width);
        let v_stride = v_plane.stride().max(chroma_width);

        let y_required = y_stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("i420 y stride overflow".into()))?;
        let u_required = u_stride
            .checked_mul(chroma_height)
            .ok_or_else(|| CodecError::Codec("i420 u stride overflow".into()))?;
        let v_required = v_stride
            .checked_mul(chroma_height)
            .ok_or_else(|| CodecError::Codec("i420 v stride overflow".into()))?;
        if y_plane.data().len() < y_required
            || u_plane.data().len() < u_required
            || v_plane.data().len() < v_required
        {
            return Err(CodecError::Codec("i420 plane buffer too short".into()));
        }

        let row_bytes = width
            .checked_mul(3)
            .ok_or_else(|| CodecError::Codec("i420 output overflow".into()))?;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("i420 output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("i420 dst buffer too short".into()));
        }
        let dst = &mut dst[..out_len];

        let y_data = &y_plane.data()[..y_required];
        let u_data = &u_plane.data()[..u_required];
        let v_data = &v_plane.data()[..v_required];
        let planar = YuvPlanarImage {
            y_plane: y_data,
            y_stride: y_stride as u32,
            u_plane: u_data,
            u_stride: u_stride as u32,
            v_plane: v_data,
            v_stride: v_stride as u32,
            width: width as u32,
            height: height as u32,
        };
        let (range, matrix) = crate::decoder::raw::yuv_colorspace(color);
        if STYX_YUV_ROWS
            || yuvutils_rs::yuv420_to_rgb(&planar, dst, row_bytes as u32, range, matrix).is_err()
        {
            let m = styx_core::simd::YuvMatrix::for_color(color);
            crate::par::for_each_row(&mut dst[..], row_bytes, |y, dst_line| {
                let y_line = &y_plane.data()[y * y_stride..];
                let u_line = &u_plane.data()[(y / 2) * u_stride..];
                let v_line = &v_plane.data()[(y / 2) * v_stride..];
                styx_core::simd::i420_rgb_row(y_line, u_line, v_line, dst_line, width, &m);
            });
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

impl Codec for I420ToRgbDecoder {
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
impl ImageDecode for I420ToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}
