//! A frame's Y plane, rows tightly packed (the Eidos raw grey layout: `width * height` bytes,
//! row-major, no padding).

use styx::prelude::*;

/// Copy `frame`'s luma into `out` (resized to `width * height`); returns the size. Grey frames
/// and the Y plane of planar YUV are copied row by row without their stride padding; packed
/// 4:2:2 has its Y bytes picked out. A raw 8-bit sensor frame (`--raw`: grey or 8-bit Bayer) is
/// copied as it is.
pub fn luma_into(frame: &FrameLease, out: &mut Vec<u8>) -> Result<(u32, u32), String> {
    let format = frame.meta().format;
    let (w, h) = (
        format.resolution.width.get() as usize,
        format.resolution.height.get() as usize,
    );
    let planes = frame.planes();
    let plane = planes.first().ok_or("frame without planes")?;
    let data = plane.data();
    out.clear();
    out.reserve(w * h);
    let code = format.code;
    // Bytes per row the Y samples span, and where the first Y byte of a pixel pair is.
    let (row_bytes, packed_y) = if crate::source::RAW_FORMATS.contains(&code)
        || [
            FourCc::NV12,
            FourCc::NV21,
            FourCc::I420,
            FourCc::YU12,
            FourCc::YV12,
            FourCc::new(*b"NV16"),
            FourCc::new(*b"NV61"),
        ]
        .contains(&code)
    {
        (w, None)
    } else if [FourCc::YUYV, FourCc::YVYU].contains(&code) {
        (w * 2, Some(0))
    } else if [FourCc::UYVY, FourCc::VYUY].contains(&code) {
        (w * 2, Some(1))
    } else {
        return Err(format!(
            "{code} frames have no luma plane to record (ask for grey frames)"
        ));
    };
    let stride = if plane.stride() == 0 {
        row_bytes
    } else {
        plane.stride()
    };
    if stride < row_bytes || data.len() < stride * (h - 1) + row_bytes {
        return Err(format!(
            "{code} {w}x{h} frame too short: {} bytes, stride {stride}",
            data.len()
        ));
    }
    for y in 0..h {
        let row = &data[y * stride..y * stride + row_bytes];
        match packed_y {
            None => out.extend_from_slice(row),
            Some(first) => out.extend(row.iter().skip(first).step_by(2)),
        }
    }
    Ok((w as u32, h as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(code: FourCc, w: u32, h: u32, stride: usize, bytes: &[u8]) -> FrameLease {
        let format = MediaFormat::new(code, Resolution::new(w, h).unwrap(), ColorSpace::Srgb);
        let mut buf = BufferPool::with_limits(1, bytes.len(), 0).lease();
        buf.resize(bytes.len());
        buf.as_mut_slice().copy_from_slice(bytes);
        FrameLease::single_plane(FrameMeta::new(format, 1), buf, bytes.len(), stride)
    }

    #[test]
    fn strips_row_padding() {
        // 3x2 grey in rows of 4.
        let f = frame(FourCc::GREY, 3, 2, 4, &[1, 2, 3, 99, 4, 5, 6, 99]);
        let mut out = Vec::new();
        assert_eq!(luma_into(&f, &mut out).unwrap(), (3, 2));
        assert_eq!(out, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn picks_y_from_packed_422() {
        let f = frame(FourCc::YUYV, 2, 1, 4, &[10, 128, 20, 129]);
        let mut out = Vec::new();
        luma_into(&f, &mut out).unwrap();
        assert_eq!(out, [10, 20]);
        let f = frame(FourCc::UYVY, 2, 1, 4, &[128, 10, 129, 20]);
        luma_into(&f, &mut out).unwrap();
        assert_eq!(out, [10, 20]);
    }

    #[test]
    fn refuses_formats_without_luma() {
        let f = frame(FourCc::RG24, 1, 1, 3, &[1, 2, 3]);
        assert!(luma_into(&f, &mut Vec::new()).is_err());
    }
}
