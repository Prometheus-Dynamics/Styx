//! Raw format decoders (pixel format conversions).

use styx_core::prelude::ColorSpace;

mod bayer;
mod bgr;
mod bgra;
mod i420;
mod mono;
mod nv12;
mod passthrough;
mod rgb48;
mod rgba;
mod yuv;
mod yuv420p;
mod yuyv;

pub use bayer::{BayerToRgbDecoder, bayer_decoder_for, bayer_info};
pub use bgr::BgrToRgbDecoder;
pub use bgra::BgraToRgbDecoder;
pub use i420::I420ToRgbDecoder;
pub use mono::{Mono8ToRgbDecoder, Mono16ToRgbDecoder};
pub use nv12::{Nv12ToBgrDecoder, Nv12ToLumaDecoder, Nv12ToRgbDecoder};
pub use passthrough::PassthroughDecoder;
pub use rgb48::Rgb48ToRgbDecoder;
pub use rgba::RgbaToRgbDecoder;
pub use yuv::{NvToRgbDecoder, Packed422ToRgbDecoder, PlanarYuvToRgbDecoder};
pub use yuv420p::Yuv420pToRgbDecoder;
pub use yuyv::{YuyvToLumaDecoder, YuyvToRgbDecoder};

#[derive(Clone, Copy)]
struct YuvCoeffs {
    r_v: i32,
    g_u: i32,
    g_v: i32,
    b_u: i32,
    full_range: bool,
}

const BT709: YuvCoeffs = YuvCoeffs {
    r_v: 459,
    g_u: 55,
    g_v: 136,
    b_u: 541,
    full_range: false,
};

// Full-range Rec.601 coefficients (Y range 0..255).
const BT601_FULL: YuvCoeffs = YuvCoeffs {
    r_v: 359,
    g_u: 88,
    g_v: 183,
    b_u: 454,
    full_range: true,
};

const BT2020: YuvCoeffs = YuvCoeffs {
    r_v: 430,
    g_u: 48,
    g_v: 166,
    b_u: 549,
    full_range: false,
};

/// Integer conversion with clamping using limited-range YUV coefficients.
#[inline(always)]
pub(crate) fn yuv_to_rgb(y: i32, u: i32, v: i32, color: ColorSpace) -> (u8, u8, u8) {
    let coeffs = match color {
        ColorSpace::Bt709 => BT709,
        ColorSpace::Bt2020 => BT2020,
        // `Srgb` in our metadata means "full-range output" (libcamera frequently reports sYCC).
        // libcamera's sYCC uses a Rec.601 YCbCr matrix with full-range.
        ColorSpace::Srgb => BT601_FULL,
        // Default unknown to limited-range Rec.709; BT.601 assumptions tend to skew heavily.
        ColorSpace::Unknown => BT709,
    };
    let d = u - 128;
    let e = v - 128;
    let (c, scale) = if coeffs.full_range {
        (y.max(0), 256)
    } else {
        (y.saturating_sub(16).max(0), 298)
    };
    let r = (scale * c + coeffs.r_v * e + 128) >> 8;
    let g = (scale * c - coeffs.g_u * d - coeffs.g_v * e + 128) >> 8;
    let b = (scale * c + coeffs.b_u * d + 128) >> 8;
    (
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
    )
}
