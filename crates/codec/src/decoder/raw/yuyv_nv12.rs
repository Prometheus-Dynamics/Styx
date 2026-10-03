//! YUYV (4:2:2 packed) to NV12 (4:2:0 semi-planar): the luma as is, the chroma of each pair
//! of rows averaged. No colour conversion, so the colour space is kept.

use smallvec::smallvec;
use styx_core::prelude::*;

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError};

/// CPU YUYV422 → NV12 converter (USB cameras to consumers of NV12).
pub struct YuyvToNv12Decoder {
    descriptor: CodecDescriptor,
    y_pool: BufferPool,
    uv_pool: BufferPool,
}

fn layouts(res: Resolution) -> [PlaneLayout; 2] {
    let (w, h) = (res.width.get() as usize, res.height.get() as usize);
    [
        PlaneLayout {
            offset: 0,
            len: w * h,
            stride: w,
        },
        PlaneLayout {
            offset: 0,
            len: w * h.div_ceil(2),
            stride: w,
        },
    ]
}

impl YuyvToNv12Decoder {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        let luma = max_width as usize * max_height as usize;
        Self {
            descriptor: crate::decoder::raw::raw_decoder_descriptor(
                FourCc::YUYV,
                FourCc::NV12,
                "yuyv2nv12",
                "yuyv-nv12",
            ),
            y_pool: BufferPool::lazy(luma, 4),
            uv_pool: BufferPool::lazy(luma.div_ceil(2), 4),
        }
    }

    /// Converts into tightly packed planes (`y`: width × height, `uv`: width × ⌈height / 2⌉).
    pub fn decode_into(
        &self,
        input: &FrameLease,
        y: &mut [u8],
        uv: &mut [u8],
    ) -> Result<FrameMeta, CodecError> {
        let meta = input.meta();
        if meta.format.code != FourCc::YUYV {
            return Err(CodecError::FormatMismatch {
                expected: FourCc::YUYV,
                actual: meta.format.code,
            });
        }
        let (w, h) = (
            meta.format.resolution.width.get() as usize,
            meta.format.resolution.height.get() as usize,
        );
        if w % 2 != 0 {
            return Err(CodecError::Codec("yuyv frame of odd width".into()));
        }
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("yuyv frame missing plane".into()))?;
        let row = w * 2;
        let stride = plane.stride().max(row);
        let src = plane.data();
        if src.len() < stride * (h - 1) + row {
            return Err(CodecError::Codec("yuyv plane buffer too short".into()));
        }
        if y.len() < w * h || uv.len() < w * h.div_ceil(2) {
            return Err(CodecError::Codec("nv12 output buffers too short".into()));
        }
        for r in 0..h {
            styx_core::simd::yuyv_luma_row(&src[r * stride..][..row], &mut y[r * w..][..w], w);
        }
        for r in 0..h.div_ceil(2) {
            let a = &src[2 * r * stride..][..row];
            // An odd last row pairs with itself.
            let b = &src[(2 * r + 1).min(h - 1) * stride..][..row];
            let out = &mut uv[r * w..][..w];
            for (i, o) in out.chunks_exact_mut(2).enumerate() {
                let (u, v) = (4 * i + 1, 4 * i + 3);
                o[0] = (u16::from(a[u]) + u16::from(b[u])).div_ceil(2) as u8;
                o[1] = (u16::from(a[v]) + u16::from(b[v])).div_ceil(2) as u8;
            }
        }
        let mut out = meta.clone();
        out.format = MediaFormat::new(FourCc::NV12, meta.format.resolution, meta.format.color);
        Ok(out)
    }
}

impl Codec for YuyvToNv12Decoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        let [yl, uvl] = layouts(input.meta().format.resolution);
        let (mut y, mut uv) = (self.y_pool.lease(), self.uv_pool.lease());
        y.resize(yl.len);
        uv.resize(uvl.len);
        let meta = self.decode_into(&input, y.as_mut_slice(), uv.as_mut_slice())?;
        Ok(FrameLease::multi_plane(
            meta,
            smallvec![y, uv],
            smallvec![yl, uvl],
        ))
    }

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        input: &FrameLease,
        pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        let [yl, mut uvl] = layouts(input.meta().format.resolution);
        uvl.offset = yl.len;
        let map = |e: FrameExportError| CodecError::Codec(e.to_string());
        let mut lease = pool.lease().map_err(map)?;
        lease.try_resize(yl.len + uvl.len).map_err(map)?;
        let (y, uv) = lease.as_mut_slice().split_at_mut(yl.len);
        let meta = self.decode_into(input, y, uv)?;
        FrameLease::multi_plane_shared(meta, lease, smallvec![yl, uvl])
            .map(Some)
            .map_err(map)
    }
}

#[cfg(feature = "image")]
impl ImageDecode for YuyvToNv12Decoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luma_is_kept_and_chroma_averaged_over_row_pairs() {
        // 4x3 YUYV: rows of Y0 U Y1 V Y2 U Y3 V with per-row chroma.
        let rows: [[u8; 8]; 3] = [
            [10, 100, 11, 200, 12, 50, 13, 60],
            [20, 102, 21, 202, 22, 52, 23, 62],
            [30, 7, 31, 9, 32, 11, 33, 13],
        ];
        let bytes: Vec<u8> = rows.concat();
        let mut buf = BufferPool::with_limits(1, bytes.len(), 1).lease();
        buf.resize(bytes.len());
        buf.as_mut_slice().copy_from_slice(&bytes);
        let res = Resolution::new(4, 3).unwrap();
        let frame = FrameLease::single_plane(
            FrameMeta::new(MediaFormat::new(FourCc::YUYV, res, ColorSpace::Srgb), 5),
            buf,
            bytes.len(),
            8,
        );
        let out = YuyvToNv12Decoder::new(4, 3).process(frame).unwrap();
        assert_eq!(out.meta().format.code, FourCc::NV12);
        assert_eq!(out.meta().format.color, ColorSpace::Srgb);
        assert_eq!(out.meta().timestamp, 5);
        let planes = out.planes();
        assert_eq!(
            planes[0].data(),
            &[10, 11, 12, 13, 20, 21, 22, 23, 30, 31, 32, 33]
        );
        // Rows 0+1 averaged (rounded up), row 2 with itself.
        assert_eq!(planes[1].data(), &[101, 201, 51, 61, 7, 9, 11, 13]);
    }
}
