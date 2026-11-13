//! YUV to RGB24 in 8-bit fixed point, and the row kernels the converters run.
//!
//! The formula is dev's scalar oracle (`styx_codec_api::simd::YuvMatrix` on dev): for each pixel,
//! `c = y_scale * max(y - y_offset, 0) + 128`, `d = u - 128`, `e = v - 128`; red is `c + r_v * e`,
//! green `c - g_u * d - g_v * e`, blue `c + b_u * d`; each is shifted right by 8 and clamped to
//! 0..=255. dev's tests pin it to the older per-decoder formula for every Y, U and V.
//!
//! SIMD kernels (NEON, RVV, x86) would slot in as alternative bodies of these row functions,
//! selected per target; the scalar body here is the reference they must match.

use styx_format::ColorSpace;

/// One YUV to RGB matrix: limited or full range, and the coefficients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct YuvMatrix {
    pub y_offset: u8,
    pub y_scale: u16,
    pub r_v: u16,
    pub g_u: u16,
    pub g_v: u16,
    pub b_u: u16,
}

impl YuvMatrix {
    /// Limited-range Rec.709 (dev's `BT709`).
    pub const BT709: Self = Self::limited(459, 55, 136, 541);
    /// Limited-range Rec.2020 (dev's `BT2020`).
    pub const BT2020: Self = Self::limited(430, 48, 166, 549);
    /// Limited-range Rec.601. Not in dev: dev's colour space has no Bt601 variant. These are the
    /// standard 8-bit coefficients (1.596, 0.392, 0.813, 2.017 times 256), flagged in the PR.
    pub const BT601_LIMITED: Self = Self::limited(409, 100, 208, 516);
    /// Full-range Rec.601 (sYCC, JFIF; dev's `BT601_FULL`).
    pub const BT601_FULL: Self = Self {
        y_offset: 0,
        y_scale: 256,
        r_v: 359,
        g_u: 88,
        g_v: 183,
        b_u: 454,
    };

    const fn limited(r_v: u16, g_u: u16, g_v: u16, b_u: u16) -> Self {
        Self {
            y_offset: 16,
            y_scale: 298,
            r_v,
            g_u,
            g_v,
            b_u,
        }
    }

    /// The matrix for a frame labelled `color`. `Srgb` YUV is full-range BT.601 (sYCC);
    /// `Unknown` is taken as limited BT.709, as dev does.
    pub const fn for_color(color: ColorSpace) -> Self {
        match color {
            ColorSpace::Bt709 | ColorSpace::Unknown => Self::BT709,
            ColorSpace::Bt2020 => Self::BT2020,
            ColorSpace::Bt601 => Self::BT601_LIMITED,
            ColorSpace::Srgb => Self::BT601_FULL,
        }
    }

    /// One pixel.
    #[inline(always)]
    pub fn rgb(&self, y: u8, u: u8, v: u8) -> [u8; 3] {
        let c = i32::from(self.y_scale) * i32::from(y.saturating_sub(self.y_offset)) + 128;
        let (d, e) = (i32::from(u) - 128, i32::from(v) - 128);
        let r = c + i32::from(self.r_v) * e;
        let g = c - i32::from(self.g_u) * d - i32::from(self.g_v) * e;
        let b = c + i32::from(self.b_u) * d;
        [r, g, b].map(|x| (x >> 8).clamp(0, 255) as u8)
    }
}

/// NV12 row to RGB24: `width` luma bytes and `width.div_ceil(2)` interleaved UV pairs.
pub fn nv12_rgb_row(y: &[u8], uv: &[u8], dst: &mut [u8], width: usize, m: &YuvMatrix) {
    for (i, px) in dst[..width * 3]
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .enumerate()
    {
        *px = m.rgb(y[i], uv[i / 2 * 2], uv[i / 2 * 2 + 1]);
    }
}

/// I420 row to RGB24: `width` luma bytes and `width.div_ceil(2)` of each chroma.
pub fn i420_rgb_row(y: &[u8], u: &[u8], v: &[u8], dst: &mut [u8], width: usize, m: &YuvMatrix) {
    for (i, px) in dst[..width * 3]
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .enumerate()
    {
        *px = m.rgb(y[i], u[i / 2], v[i / 2]);
    }
}

/// YUYV row to RGB24: `width.div_ceil(2)` four-byte Y0 U Y1 V groups.
pub fn yuyv_rgb_row(src: &[u8], dst: &mut [u8], width: usize, m: &YuvMatrix) {
    for (i, px) in dst[..width * 3]
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .enumerate()
    {
        let g = &src[i / 2 * 4..][..4];
        *px = m.rgb(g[i % 2 * 2], g[1], g[3]);
    }
}
