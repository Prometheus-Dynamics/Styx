//! Plane layouts of one V4L2 buffer: the bytes-per-line the driver negotiated, laid over the
//! format's planes from the format crate's geometry.

use styx_format::{FourCc, PlaneGeometry, plane_geometry};
use styx_frame::{Layouts, PlaneLayout};

use crate::Error;

/// The layouts of an uncompressed frame of `code` at `width` x `height`, where the first plane
/// has `bytes_per_line` bytes per row. Later planes keep their size relative to the first:
/// chroma rows of NV12 are as long as luma rows, chroma rows of I420 half as long.
pub(crate) fn uncompressed(
    code: FourCc,
    width: u32,
    height: u32,
    bytes_per_line: u32,
) -> Result<Layouts, Error> {
    let geometry = plane_geometry(code, width, height)
        .map_err(|e| Error::Unsupported(format!("no layout for {code}: {e:?}")))?;
    let planes: Vec<PlaneGeometry> = geometry.iter().copied().collect();
    let first = planes
        .first()
        .ok_or_else(|| Error::Unsupported(format!("{code} has no planes")))?;
    let line = bytes_per_line as usize;
    if first.row_bytes == 0 || line < first.row_bytes {
        return Err(Error::Unsupported(format!(
            "{code}: {line} bytes per line is less than the {} a row needs",
            first.row_bytes
        )));
    }
    let mut items: Vec<PlaneLayout> = Vec::with_capacity(planes.len());
    let mut offset = 0usize;
    for (i, plane) in planes.iter().enumerate() {
        let stride = if i == 0 {
            line
        } else {
            line.checked_mul(plane.row_bytes)
                .ok_or_else(|| Error::Unsupported("plane stride overflows".into()))?
                .div_ceil(first.row_bytes)
        };
        if stride < plane.row_bytes {
            return Err(Error::Unsupported(format!(
                "{code}: plane {i} stride is short"
            )));
        }
        let len = stride
            .checked_mul(plane.rows)
            .ok_or_else(|| Error::Unsupported("plane size overflows".into()))?;
        items.push(PlaneLayout::new(offset, stride, len));
        offset = offset
            .checked_add(len)
            .ok_or_else(|| Error::Unsupported("frame size overflows".into()))?;
    }
    Layouts::new(&items).ok_or_else(|| Error::Unsupported(format!("{code}: too many planes")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_luma_and_interleaved_chroma_share_the_line_length() {
        let layouts = uncompressed(FourCc::NV12, 640, 480, 640).expect("nv12");
        assert_eq!(layouts.len(), 2);
        assert_eq!(
            (layouts[0].offset, layouts[0].stride, layouts[0].len),
            (0, 640, 307_200)
        );
        assert_eq!(
            (layouts[1].offset, layouts[1].stride, layouts[1].len),
            (307_200, 640, 153_600)
        );
        assert_eq!(layouts.span(), Some(460_800));
    }

    #[test]
    fn a_padded_line_pads_every_plane() {
        let layouts = uncompressed(FourCc::NV12, 640, 480, 704).expect("nv12 padded");
        assert_eq!(layouts[1].stride, 704);
        assert_eq!(layouts[1].offset, 704 * 480);
    }

    #[test]
    fn i420_chroma_rows_are_half_the_luma_line() {
        let layouts = uncompressed(FourCc::I420, 640, 480, 640).expect("i420");
        assert_eq!(layouts.len(), 3);
        assert_eq!((layouts[1].stride, layouts[1].len), (320, 76_800));
        assert_eq!(layouts[2].offset, 384_000);
        assert_eq!(layouts.span(), Some(460_800));
    }

    #[test]
    fn packed_yuyv_is_one_plane_of_two_bytes_a_pixel() {
        let layouts = uncompressed(FourCc::YUYV, 640, 480, 1280).expect("yuyv");
        assert_eq!(layouts.len(), 1);
        assert_eq!((layouts[0].stride, layouts[0].len), (1280, 614_400));
    }

    #[test]
    fn a_line_shorter_than_a_row_is_refused() {
        assert!(uncompressed(FourCc::YUYV, 640, 480, 1279).is_err());
        assert!(uncompressed(FourCc::NV12, 640, 480, 0).is_err());
    }
}
