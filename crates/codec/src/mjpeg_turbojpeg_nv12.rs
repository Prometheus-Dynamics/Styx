//! MJPEG → NV12 (4:2:0 semi-planar) decoding with libturbojpeg's planar YUV output.
//!
//! A camera's MJPEG frames are 4:2:2 (many webcams: C270, C922) or 4:2:0 JPEGs: libjpeg-turbo
//! writes their luma and chroma planes as they are (no colour conversion), and the chroma is
//! averaged into NV12's 4:2:0 layout. Without this decoder an NV12 consumer of a USB camera gets
//! only the uncompressed modes (YUYV at full size, 221 MB/s for a 2560x1440 mode on a 480 Mbit/s
//! bus), where the camera's MJPEG modes would stream.

use std::cell::RefCell;

use smallvec::smallvec;
use styx_core::prelude::*;

use std::ffi::c_int;

use turbojpeg::raw;

use crate::turbojpeg_raw::TjDecompressor;
use crate::{Codec, CodecDescriptor, CodecError, CodecKind, DEFAULT_CODEC_POOL_SPARE};

thread_local! {
    /// Per-thread scratch for the two chroma planes (decoded, then interleaved into NV12).
    static CHROMA: RefCell<(Vec<u8>, Vec<u8>)> = const { RefCell::new((Vec::new(), Vec::new())) };
}

/// NV12 layouts for `width` x `height`: the luma (stride `width`), then the interleaved chroma
/// rows (`width` bytes each, ⌈height / 2⌉ of them).
fn layouts(width: usize, height: usize) -> [PlaneLayout; 2] {
    [
        PlaneLayout {
            offset: 0,
            len: width * height,
            stride: width,
        },
        PlaneLayout {
            offset: 0,
            len: width * height.div_ceil(2),
            stride: width,
        },
    ]
}

/// MJPEG → NV12 decoder using libturbojpeg (`turbojpeg-nv12`).
pub struct TurbojpegNv12Decoder {
    descriptor: CodecDescriptor,
    y_pool: BufferPool,
    uv_pool: BufferPool,
}

impl Default for TurbojpegNv12Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl TurbojpegNv12Decoder {
    pub fn new() -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: FourCc::MJPG,
                output: FourCc::NV12,
                name: "mjpeg",
                impl_name: "turbojpeg-nv12",
            },
            // Frames are leased at their exact decoded length, as the luma decoder does.
            y_pool: BufferPool::lazy(0, DEFAULT_CODEC_POOL_SPARE),
            uv_pool: BufferPool::lazy(0, DEFAULT_CODEC_POOL_SPARE),
        }
    }

    /// Decodes into `y` and `uv`, tightly packed as [`layouts`] give them. Refuses frames NV12
    /// cannot carry: odd sizes, and JPEGs that are not 4:2:0.
    fn decode_into(
        &self,
        input: &FrameLease,
        y: &mut [u8],
        uv: &mut [u8],
    ) -> Result<FrameMeta, CodecError> {
        let meta = input.meta();
        if meta.format.code != FourCc::MJPG {
            return Err(CodecError::FormatMismatch {
                expected: FourCc::MJPG,
                actual: meta.format.code,
            });
        }
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("mjpeg frame missing plane".into()))?;
        let tj = TjDecompressor::new()?;
        let header = tj.read_header(plane.data())?;
        crate::check_decoded_size(meta.format.resolution, header.width, header.height)?;
        let (w, h) = (header.width as usize, header.height as usize);
        if w % 2 != 0 || h % 2 != 0 {
            return Err(CodecError::Codec(
                "NV12 from MJPEG needs even dimensions".into(),
            ));
        }
        // Chroma as the JPEG holds it: 4:2:0 (`cw` x `ch` = w/2 x h/2) or 4:2:2 (w/2 x h).
        let subsamp = tj.subsamp();
        let (cw, ch) = if subsamp == raw::TJSAMP_TJSAMP_420 as c_int {
            (w / 2, h / 2)
        } else if subsamp == raw::TJSAMP_TJSAMP_422 as c_int {
            (w / 2, h)
        } else {
            return Err(CodecError::Codec(
                "NV12 from MJPEG needs 4:2:0 or 4:2:2 subsampling".into(),
            ));
        };
        CHROMA.with(|scratch| {
            let mut scratch = scratch.borrow_mut();
            let (cb, cr) = &mut *scratch;
            cb.resize(cw * ch, 0);
            cr.resize(cw * ch, 0);
            tj.decompress_to_yuv_planes(
                plane.data(),
                [&mut y[..w * h], &mut cb[..], &mut cr[..]],
                [w, cw, cw],
            )?;
            for r in 0..h / 2 {
                // The chroma row of NV12 row `r`: one source row (4:2:0), or the mean of two
                // (4:2:2 rows 2r and 2r+1).
                let rows = if subsamp == raw::TJSAMP_TJSAMP_420 as c_int {
                    [r, r]
                } else {
                    [2 * r, 2 * r + 1]
                };
                let out = &mut uv[r * w..][..w];
                for (i, pair) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    let at = |row: usize, plane: &[u8]| u16::from(plane[row * cw + i]);
                    let (u, v) = (&cb[..], &cr[..]);
                    let mean = |plane: &[u8]| {
                        if rows[0] == rows[1] {
                            at(rows[0], plane)
                        } else {
                            (at(rows[0], plane) + at(rows[1], plane)).div_ceil(2)
                        }
                    };
                    pair[0] = mean(u) as u8;
                    pair[1] = mean(v) as u8;
                }
            }
            Ok(())
        })?;
        let mut out = meta.clone();
        out.format = MediaFormat::new(FourCc::NV12, meta.format.resolution, meta.format.color);
        Ok(out)
    }
}

impl Codec for TurbojpegNv12Decoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        let res = input.meta().format.resolution;
        let [yl, uvl] = layouts(res.width.get() as usize, res.height.get() as usize);
        let (mut y, mut uv) = (
            self.y_pool.lease_sized(yl.len),
            self.uv_pool.lease_sized(uvl.len),
        );
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
        let res = input.meta().format.resolution;
        let [yl, mut uvl] = layouts(res.width.get() as usize, res.height.get() as usize);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mjpeg_turbojpeg::TurbojpegEncoder;

    /// A frame of `code` (NV12 or YUYV, one plane) that is uniform: luma 81, chroma 90 and 240.
    fn uniform(code: FourCc, w: u32, h: u32) -> FrameLease {
        let (w, h) = (w as usize, h as usize);
        let mut bytes = Vec::new();
        match code {
            FourCc::NV12 => {
                bytes.resize(w * h, 81);
                for _ in 0..(w * h / 4) {
                    bytes.extend_from_slice(&[90, 240]);
                }
            }
            _ => {
                for _ in 0..h {
                    for _ in 0..w / 2 {
                        bytes.extend_from_slice(&[81, 90, 81, 240]);
                    }
                }
            }
        }
        let fmt = MediaFormat::new(
            code,
            Resolution::new(w as u32, h as u32).unwrap(),
            ColorSpace::Unknown,
        );
        let stride = if code == FourCc::NV12 { w } else { w * 2 };
        let mut buf = BufferPool::with_limits(1, bytes.len(), 1).lease();
        buf.resize(bytes.len());
        buf.as_mut_slice().copy_from_slice(&bytes);
        FrameLease::single_plane(FrameMeta::new(fmt, 5), buf, bytes.len(), stride)
    }

    /// A 4:2:0 MJPEG of a uniform NV12 frame decodes to NV12 planes of the same colour.
    #[test]
    fn mjpeg_420_decodes_to_nv12_planes() {
        let jpeg = TurbojpegEncoder::new(FourCc::NV12, 95)
            .process(uniform(FourCc::NV12, 64, 48))
            .expect("encode nv12 as mjpeg");
        let nv12 = TurbojpegNv12Decoder::new()
            .process(jpeg)
            .expect("decode mjpeg to nv12");
        assert_eq!(nv12.meta().format.code, FourCc::NV12);
        assert_eq!(nv12.meta().timestamp, 5);
        let planes = nv12.planes();
        assert_eq!(planes.len(), 2);
        let (y, uv) = (planes[0].data(), planes[1].data());
        assert!(y[..64 * 48].iter().all(|&v| v.abs_diff(81) <= 4), "luma");
        for pair in uv[..64 * 24].as_chunks::<2>().0 {
            assert!(
                pair[0].abs_diff(90) <= 6 && pair[1].abs_diff(240) <= 6,
                "chroma {pair:?}"
            );
        }
    }

    /// YUYV encodes 4:2:2 (as webcams' MJPEG does): its chroma rows average into NV12's.
    #[test]
    fn mjpeg_422_decodes_to_nv12_planes() {
        let jpeg = TurbojpegEncoder::new(FourCc::YUYV, 95)
            .process(uniform(FourCc::YUYV, 64, 48))
            .expect("encode yuyv as mjpeg");
        let nv12 = TurbojpegNv12Decoder::new()
            .process(jpeg)
            .expect("decode 4:2:2 mjpeg to nv12");
        let planes = nv12.planes();
        let (y, uv) = (planes[0].data(), planes[1].data());
        assert!(y[..64 * 48].iter().all(|&v| v.abs_diff(81) <= 4), "luma");
        for pair in uv[..64 * 24].as_chunks::<2>().0 {
            assert!(
                pair[0].abs_diff(90) <= 6 && pair[1].abs_diff(240) <= 6,
                "chroma {pair:?}"
            );
        }
    }
}
