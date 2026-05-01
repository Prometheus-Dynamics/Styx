use std::sync::Arc;

use styx_core::prelude::*;

use crate::{Codec, CodecDescriptor, CodecError};

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};

#[path = "bayer_demosaic.rs"]
mod bayer_demosaic;
#[path = "bayer_info.rs"]
mod bayer_info;
#[path = "bayer_unpack.rs"]
mod bayer_unpack;

use bayer_demosaic::{demosaic_bilinear_to_rg24, demosaic_bilinear_u16_le};
pub use bayer_info::{BayerInfo, bayer_info};
use bayer_unpack::{min_stride, unpack_mipi_packed_to_u16_le};
#[cfg(test)]
use bayer_unpack::{sample_at, unpack_raw10_row};

#[derive(Clone, Copy, PartialEq, Eq)]
// Bayer pattern names are conventional four-letter sensor layout identifiers.
#[allow(clippy::upper_case_acronyms)]
pub(super) enum BayerPattern {
    RGGB,
    BGGR,
    GBRG,
    GRBG,
}

pub struct BayerToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    packed_pool: BufferPool,
    info: BayerInfo,
}

impl BayerToRgbDecoder {
    pub fn new(input: FourCc, info: BayerInfo, max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        let packed_bytes = max_width as usize * max_height as usize * 2;
        Self {
            descriptor: crate::decoder::raw::raw_decoder_descriptor(
                input,
                FourCc::RG24,
                "bayer2rgb",
                "bayer-bilinear",
            ),
            pool: BufferPool::lazy(bytes, 4),
            packed_pool: BufferPool::lazy(packed_bytes, 4),
            info,
        }
    }

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
            .ok_or_else(|| CodecError::Codec("bayer frame missing plane".into()))?;

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        if width < 2 || height < 2 {
            return Err(CodecError::Codec("bayer frame too small".into()));
        }

        let stride = plane.stride().max(min_stride(
            width,
            self.info.bit_depth,
            self.info.bytes_per_sample,
        ));
        let required = stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("bayer stride overflow".into()))?;
        if plane.data().len() < required {
            return Err(CodecError::Codec("bayer plane buffer too short".into()));
        }

        let row_bytes = width
            .checked_mul(3)
            .ok_or_else(|| CodecError::Codec("bayer output overflow".into()))?;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("bayer output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("bayer dst buffer too short".into()));
        }

        let dst = &mut dst[..out_len];
        let data = plane.data();
        if self.info.bytes_per_sample == 0 {
            let mut packed = self.packed_pool.lease();
            let packed_len = width
                .checked_mul(height)
                .and_then(|px| px.checked_mul(2))
                .ok_or_else(|| CodecError::Codec("bayer packed buffer overflow".into()))?;
            unsafe { packed.resize_uninit(packed_len) };
            let packed_u16 = unsafe {
                std::slice::from_raw_parts_mut(
                    packed.as_mut_slice().as_mut_ptr() as *mut u16,
                    width * height,
                )
            };
            unpack_mipi_packed_to_u16_le(
                packed_u16,
                data,
                stride,
                width,
                height,
                self.info.bit_depth,
            );
            demosaic_bilinear_u16_le(
                dst,
                packed.as_slice(),
                width,
                width,
                height,
                self.info.pattern,
                self.info.bit_depth,
            );
        } else {
            demosaic_bilinear_to_rg24(
                dst,
                data,
                stride,
                width,
                height,
                self.info.pattern,
                self.info.bit_depth,
                self.info.bytes_per_sample,
            );
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

impl Codec for BayerToRgbDecoder {
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
impl ImageDecode for BayerToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

pub fn bayer_decoder_for(
    fourcc: FourCc,
    info: BayerInfo,
    max_width: u32,
    max_height: u32,
) -> Arc<dyn Codec> {
    Arc::new(BayerToRgbDecoder::new(fourcc, info, max_width, max_height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack_raw10_4px(p0: u16, p1: u16, p2: u16, p3: u16) -> [u8; 5] {
        let b0 = (p0 & 0xff) as u8;
        let b1 = (p1 & 0xff) as u8;
        let b2 = (p2 & 0xff) as u8;
        let b3 = (p3 & 0xff) as u8;
        let b4 = ((p0 >> 8) as u8 & 0x03)
            | (((p1 >> 8) as u8 & 0x03) << 2)
            | (((p2 >> 8) as u8 & 0x03) << 4)
            | (((p3 >> 8) as u8 & 0x03) << 6);
        [b0, b1, b2, b3, b4]
    }

    #[test]
    fn raw10_packed_sampling_matches_values() {
        let row = pack_raw10_4px(0x000, 0x155, 0x2aa, 0x3ff);
        let stride = row.len();
        let data = row.as_slice();
        let w = 4;
        let h = 1;
        assert_eq!(sample_at(data, stride, 0, 10, 0, 0, w, h), 0x00);
        assert_eq!(sample_at(data, stride, 0, 10, 1, 0, w, h), 0x55);
        assert_eq!(sample_at(data, stride, 0, 10, 2, 0, w, h), 0xaa);
        assert_eq!(sample_at(data, stride, 0, 10, 3, 0, w, h), 0xff);
    }

    #[test]
    fn raw10_unpack_matches_values() {
        let row = pack_raw10_4px(0x000, 0x155, 0x2aa, 0x3ff);
        let mut out = [0u16; 4];
        unpack_raw10_row(&mut out, &row, 4);
        assert_eq!(u16::from_le(out[0]), 0x000);
        assert_eq!(u16::from_le(out[1]), 0x155);
        assert_eq!(u16::from_le(out[2]), 0x2aa);
        assert_eq!(u16::from_le(out[3]), 0x3ff);
    }

    #[test]
    fn packed_raw10_decode_matches_unpacked() {
        let w = 4usize;
        let h = 4usize;
        let res = Resolution::new(w as u32, h as u32).unwrap();

        let mut raw = vec![0u16; w * h];
        for y in 0..h {
            for x in 0..w {
                raw[y * w + x] = (((y * w + x) * 77) & 0x3ff) as u16;
            }
        }

        let packed_stride = 5usize;
        let mut packed = Vec::with_capacity(packed_stride * h);
        for y in 0..h {
            let row = &raw[y * w..(y + 1) * w];
            packed.extend_from_slice(&pack_raw10_4px(row[0], row[1], row[2], row[3]));
        }

        let unpacked_stride = w * 2;
        let mut unpacked = vec![0u8; unpacked_stride * h];
        for y in 0..h {
            for x in 0..w {
                let v = raw[y * w + x].to_le_bytes();
                let o = y * unpacked_stride + x * 2;
                unpacked[o] = v[0];
                unpacked[o + 1] = v[1];
            }
        }

        let packed_fourcc = FourCc::new(*b"pRAA");
        let unpacked_fourcc = FourCc::new(*b"RG10");
        let packed_info = bayer_info(packed_fourcc).unwrap();
        let unpacked_info = bayer_info(unpacked_fourcc).unwrap();
        let packed_dec = BayerToRgbDecoder::new(
            packed_fourcc,
            packed_info,
            res.width.get(),
            res.height.get(),
        );
        let unpacked_dec = BayerToRgbDecoder::new(
            unpacked_fourcc,
            unpacked_info,
            res.width.get(),
            res.height.get(),
        );

        let pool = BufferPool::with_limits(2, packed.len().max(unpacked.len()), 4);

        let mut packed_buf = pool.lease();
        packed_buf.resize(packed.len());
        packed_buf.as_mut_slice().copy_from_slice(&packed);
        let packed_frame = FrameLease::single_plane(
            FrameMeta::new(MediaFormat::new(packed_fourcc, res, ColorSpace::Unknown), 0),
            packed_buf,
            packed.len(),
            packed_stride,
        );

        let mut unpacked_buf = pool.lease();
        unpacked_buf.resize(unpacked.len());
        unpacked_buf.as_mut_slice().copy_from_slice(&unpacked);
        let unpacked_frame = FrameLease::single_plane(
            FrameMeta::new(
                MediaFormat::new(unpacked_fourcc, res, ColorSpace::Unknown),
                0,
            ),
            unpacked_buf,
            unpacked.len(),
            unpacked_stride,
        );

        let a = packed_dec.process(packed_frame).unwrap();
        let b = unpacked_dec.process(unpacked_frame).unwrap();
        let a_plane = a.planes();
        let b_plane = b.planes();
        assert_eq!(a_plane.len(), 1);
        assert_eq!(b_plane.len(), 1);
        assert_eq!(a_plane[0].data(), b_plane[0].data());
    }
}
