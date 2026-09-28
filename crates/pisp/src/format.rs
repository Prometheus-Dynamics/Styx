//! Image format helpers: strides, byte offsets of pixels, plane counts, and the common
//! formats by name.
//!
//! Ported from libpisp `src/libpisp/common/pisp_utils.cpp` (BSD-2-Clause,
//! Copyright (C) 2021 - 2023, Raspberry Pi Ltd; see the crate docs for the licence text).

use crate::uapi::ImageFormatConfig;
use crate::uapi::image_format as f;

/// Common formats, as libpisp names them.
pub mod formats {
    use crate::uapi::image_format as f;

    /// 16-bit Bayer or mono (samples in the top bits).
    pub const BAYER16: u32 = f::BPS_16;
    /// 8-bit PiSP compressed Bayer (mode 1).
    pub const PISP_COMP1: u32 = f::COMPRESSION_MODE_1;
    /// 24-bit RGB (R first in memory).
    pub const RGB888: u32 = f::THREE_CHANNEL;
    /// 32-bit RGBX.
    pub const RGBX8888: u32 = f::THREE_CHANNEL | f::BPP_32;
    /// 48-bit RGB.
    pub const RGB161616: u32 = f::THREE_CHANNEL | f::BPS_16;
    /// YUV 4:2:0 semi-planar, CbCr.
    pub const NV12: u32 = f::THREE_CHANNEL | f::BPS_8 | f::SAMPLING_420 | f::PLANARITY_SEMI_PLANAR;
    /// YUV 4:2:0 semi-planar, CrCb.
    pub const NV21: u32 = NV12 | f::ORDER_SWAPPED;
    /// YUV 4:2:0 planar.
    pub const YUV420P: u32 = f::THREE_CHANNEL | f::BPS_8 | f::SAMPLING_420 | f::PLANARITY_PLANAR;
    /// YUV 4:2:2 planar.
    pub const YUV422P: u32 = f::THREE_CHANNEL | f::BPS_8 | f::SAMPLING_422 | f::PLANARITY_PLANAR;
    /// YUV 4:4:4 planar.
    pub const YUV444P: u32 = f::THREE_CHANNEL | f::BPS_8 | f::SAMPLING_444 | f::PLANARITY_PLANAR;
    /// YUYV 4:2:2 interleaved.
    pub const YUYV: u32 = f::THREE_CHANNEL | f::BPS_8 | f::SAMPLING_422 | f::PLANARITY_INTERLEAVED;
    /// UYVY 4:2:2 interleaved.
    pub const UYVY: u32 = YUYV | f::ORDER_SWAPPED;
}

fn has(fmt: u32, mask: u32, value: u32) -> bool {
    fmt & mask == value
}

/// Byte offset of pixel column `x` in a line.
pub fn x_offset(format: u32, x: u32) -> u32 {
    if format & (f::HOG_SIGNED | f::HOG_UNSIGNED) != 0 {
        return x * if format & f::HOG_UNSIGNED != 0 {
            32
        } else {
            48
        };
    }
    if format & (f::INTEGRAL_IMAGE | f::BPP_32) != 0 {
        return x * 4;
    }
    let mut off = match format & f::BPS_MASK {
        f::BPS_16 => x * 2,
        f::BPS_12 => (x * 3).div_ceil(2),
        f::BPS_10 => (x / 3) * 4,
        _ => x,
    };
    if format & f::THREE_CHANNEL != 0 && has(format, f::PLANARITY_MASK, f::PLANARITY_INTERLEAVED) {
        off *= if has(format, f::SAMPLING_MASK, f::SAMPLING_422) {
            2
        } else {
            3
        };
    }
    off
}

/// Fills in `stride` (kept if already larger) and `stride2`, aligned to `align` bytes.
pub fn compute_stride_align(cfg: &mut ImageFormatConfig, align: u32) {
    let fmt = cfg.format;
    if fmt & f::WALLPAPER_ROLL != 0 {
        cfg.stride = (u32::from(cfg.height) * f::WALLPAPER_WIDTH) as i32;
        cfg.stride2 = cfg.stride;
        if has(fmt, f::SAMPLING_MASK, f::SAMPLING_420) {
            cfg.stride2 /= 2;
        }
        return;
    }
    let mut width = u32::from(cfg.width);
    if f::is_compressed(fmt) {
        width = (width + 7) & !7;
    }
    let computed = x_offset(fmt, width) as i32;
    if cfg.stride == 0 || cfg.stride < computed {
        cfg.stride = computed;
    }
    cfg.stride2 = 0;
    if fmt & (f::HOG_SIGNED | f::HOG_UNSIGNED) == 0 {
        let sub = has(fmt, f::SAMPLING_MASK, f::SAMPLING_422)
            || has(fmt, f::SAMPLING_MASK, f::SAMPLING_420);
        match fmt & f::PLANARITY_MASK {
            f::PLANARITY_PLANAR if sub => cfg.stride2 = cfg.stride >> 1,
            f::PLANARITY_PLANAR if f::is_three_channel(fmt) => cfg.stride2 = cfg.stride,
            f::PLANARITY_SEMI_PLANAR => cfg.stride2 = cfg.stride,
            _ => {}
        }
        let a = align as i32;
        cfg.stride = (cfg.stride + a - 1) & !(a - 1);
        cfg.stride2 = (cfg.stride2 + a - 1) & !(a - 1);
    }
}

/// Byte offsets of pixel `(x, y)` in plane 0 and in the chroma plane(s).
pub fn addr_offset(cfg: &ImageFormatConfig, x: u32, y: u32) -> (u32, u32) {
    let fmt = cfg.format;
    if fmt & f::WALLPAPER_ROLL != 0 {
        let roll = match fmt & f::BPS_MASK {
            f::BPS_8 => f::WALLPAPER_WIDTH,
            f::BPS_16 => f::WALLPAPER_WIDTH / 2,
            _ => f::WALLPAPER_WIDTH / 4 * 3,
        };
        let in_roll = x % roll;
        let in_bytes = match fmt & f::BPS_MASK {
            f::BPS_8 => in_roll,
            f::BPS_16 => in_roll * 2,
            _ => in_roll / 3 * 4,
        };
        let rolls = x / roll;
        let o1 = rolls * cfg.stride as u32 + y * f::WALLPAPER_WIDTH + in_bytes;
        let o2 = if has(fmt, f::SAMPLING_MASK, f::SAMPLING_420) {
            rolls * cfg.stride2 as u32 + y / 2 * f::WALLPAPER_WIDTH + in_bytes
        } else {
            o1
        };
        return (o1, o2);
    }
    let xb = x_offset(fmt, x);
    let o1 = y * cfg.stride as u32 + xb;
    let mut o2 = 0;
    if !has(fmt, f::PLANARITY_MASK, f::PLANARITY_INTERLEAVED) {
        let y2 = if has(fmt, f::SAMPLING_MASK, f::SAMPLING_420) {
            y / 2
        } else {
            y
        };
        let x2 = if has(fmt, f::PLANARITY_MASK, f::PLANARITY_PLANAR)
            && !has(fmt, f::SAMPLING_MASK, f::SAMPLING_444)
        {
            xb / 2
        } else {
            xb
        };
        o2 = y2 * cfg.stride2 as u32 + x2;
    }
    (o1, o2)
}

/// Number of memory planes.
pub fn num_planes(format: u32) -> usize {
    if !f::is_three_channel(format) {
        return 1;
    }
    match format & f::PLANARITY_MASK {
        f::PLANARITY_SEMI_PLANAR => 2,
        f::PLANARITY_PLANAR => 3,
        _ => 1,
    }
}

/// Size in bytes of plane `plane`.
pub fn plane_size(cfg: &ImageFormatConfig, plane: usize) -> usize {
    let stride = (if plane > 0 { cfg.stride2 } else { cfg.stride }).unsigned_abs() as usize;
    let h = if plane > 0 && has(cfg.format, f::SAMPLING_MASK, f::SAMPLING_420) {
        cfg.height as usize / 2
    } else {
        cfg.height as usize
    };
    h * stride
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(width: u16, height: u16, format: u32) -> ImageFormatConfig {
        ImageFormatConfig {
            width,
            height,
            format,
            ..Default::default()
        }
    }

    #[test]
    fn strides() {
        let mut c = img(1280, 800, formats::BAYER16);
        compute_stride_align(&mut c, 64);
        assert_eq!((c.stride, c.stride2), (2560, 0));
        let mut c = img(1280, 800, formats::NV12);
        compute_stride_align(&mut c, 64);
        assert_eq!((c.stride, c.stride2), (1280, 1280));
        let mut c = img(1000, 10, formats::RGB888);
        compute_stride_align(&mut c, 16);
        assert_eq!(c.stride, 3008);
        let mut c = img(1280, 800, formats::YUV420P);
        compute_stride_align(&mut c, 64);
        assert_eq!((c.stride, c.stride2), (1280, 640));
        let mut c = img(13, 2, formats::PISP_COMP1);
        compute_stride_align(&mut c, 16);
        assert_eq!(c.stride, 16);
    }

    #[test]
    fn offsets() {
        let mut c = img(1280, 800, formats::NV12);
        compute_stride_align(&mut c, 64);
        assert_eq!(addr_offset(&c, 640, 10), (10 * 1280 + 640, 5 * 1280 + 640));
        let mut c = img(1280, 800, formats::BAYER16);
        compute_stride_align(&mut c, 64);
        assert_eq!(addr_offset(&c, 624, 0), (1248, 0));
        let mut c = img(64, 64, formats::YUV420P);
        compute_stride_align(&mut c, 16);
        assert_eq!(addr_offset(&c, 32, 8), (8 * 64 + 32, 4 * 32 + 16));
        assert_eq!(num_planes(formats::YUV420P), 3);
        assert_eq!(plane_size(&c, 1), 32 * 32);
    }
}
