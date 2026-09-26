//! Plane layouts for planar and semi-planar YUV delivered in one single-planar V4L2 buffer.

use smallvec::{SmallVec, smallvec};
use styx_core::prelude::{ChromaSubsampling, FourCc, FrameStorageKind, PlaneLayout};

/// Y plane followed by chroma plane(s) inside one buffer, as single-planar V4L2 lays out NV12,
/// NV16, I420, YV12, ... `stride` is the Y row pitch (`bytesperline`).
///
/// Returns `None` for other formats or when `available` bytes cannot hold every plane.
pub(super) fn yuv_plane_layouts(
    code: FourCc,
    width: usize,
    height: usize,
    stride: usize,
    available: usize,
) -> Option<SmallVec<[PlaneLayout; 3]>> {
    let info = code.layout_info();
    let subsampling = info.planes.subsampling?;
    let stride = stride.max(width);
    let y_len = stride.checked_mul(height)?;
    let chroma_rows = match subsampling {
        ChromaSubsampling::Cs420 => height.div_ceil(2),
        ChromaSubsampling::Cs422 | ChromaSubsampling::Cs444 => height,
    };
    let mut layouts: SmallVec<[PlaneLayout; 3]> = smallvec![PlaneLayout {
        offset: 0,
        len: y_len,
        stride,
    }];
    match info.storage {
        FrameStorageKind::SemiPlanar => {
            // Interleaved CbCr: full-width rows for 4:2:0/4:2:2, double width for 4:4:4.
            let uv_stride = match subsampling {
                ChromaSubsampling::Cs444 => stride.checked_mul(2)?,
                _ => stride,
            };
            layouts.push(PlaneLayout {
                offset: y_len,
                len: uv_stride.checked_mul(chroma_rows)?,
                stride: uv_stride,
            });
        }
        FrameStorageKind::Planar => {
            let c_stride = match subsampling {
                ChromaSubsampling::Cs444 => stride,
                _ => stride.div_ceil(2),
            };
            let c_len = c_stride.checked_mul(chroma_rows)?;
            layouts.push(PlaneLayout {
                offset: y_len,
                len: c_len,
                stride: c_stride,
            });
            layouts.push(PlaneLayout {
                offset: y_len.checked_add(c_len)?,
                len: c_len,
                stride: c_stride,
            });
        }
        _ => return None,
    }
    let end = layouts.last().map(|l| l.offset + l.len)?;
    (end <= available).then_some(layouts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_has_full_stride_y_and_half_height_uv() {
        let layouts = yuv_plane_layouts(FourCc::NV12, 640, 480, 640, 460_800).expect("nv12");
        assert_eq!(layouts.len(), 2);
        assert_eq!(
            (layouts[0].offset, layouts[0].len, layouts[0].stride),
            (0, 307_200, 640)
        );
        assert_eq!(
            (layouts[1].offset, layouts[1].len, layouts[1].stride),
            (307_200, 153_600, 640)
        );
    }

    #[test]
    fn i420_has_two_quarter_size_chroma_planes() {
        let layouts = yuv_plane_layouts(FourCc::I420, 640, 480, 640, 460_800).expect("i420");
        assert_eq!(layouts.len(), 3);
        assert_eq!(
            (layouts[1].offset, layouts[1].len, layouts[1].stride),
            (307_200, 76_800, 320)
        );
        assert_eq!(layouts[2].offset, 384_000);
    }

    #[test]
    fn short_buffers_and_packed_formats_are_rejected() {
        assert!(yuv_plane_layouts(FourCc::NV12, 640, 480, 640, 460_799).is_none());
        assert!(yuv_plane_layouts(FourCc::YUYV, 640, 480, 1280, 614_400).is_none());
    }
}
