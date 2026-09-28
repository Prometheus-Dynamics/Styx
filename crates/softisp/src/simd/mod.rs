//! Row kernels of the software ISP, in the pattern of `styx_core::simd`: the scalar kernels
//! ([`scalar`]) are the correctness oracle; x86 (SSE2, SSSE3, AVX2, chosen at run time from the
//! CPU's features) and AArch64 NEON leaves must produce exactly the scalar result. A leaf handles
//! a prefix of the row and the dispatcher finishes it with the scalar kernel. Cargo features
//! `x86` and `neon` (on by default) compile the backends in.
//!
//! Every dispatcher returns the [`SimdBackend`] that did the vector part of the work.

pub mod scalar;

pub use styx_core::simd::{SimdBackend, X86FeatureSet, strongest_backend};

use crate::format::CfaPattern;

#[cfg(all(feature = "neon", target_arch = "aarch64"))]
mod neon;
#[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
mod x86;

#[cfg(test)]
mod tests;

/// The colours of one mosaic row: which columns are green, and whether the other colour on the
/// row (`X`; the rows above and below carry `Y`) is red.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowKind {
    pub green_even: bool,
    pub x_is_red: bool,
}

impl RowKind {
    /// The kind of row `y` of a mosaic with `pattern`.
    pub fn of(pattern: CfaPattern, y: usize) -> Self {
        let odd_row = y & 1 == 1;
        let (green_even, x_is_red) = match pattern {
            CfaPattern::Rggb => (odd_row, !odd_row),
            CfaPattern::Bggr => (odd_row, odd_row),
            CfaPattern::Grbg => (!odd_row, !odd_row),
            CfaPattern::Gbrg => (!odd_row, odd_row),
        };
        Self {
            green_even,
            x_is_red,
        }
    }

    /// Whether column `x` is green.
    #[inline(always)]
    pub fn is_green(self, x: usize) -> bool {
        (x & 1 == 0) == self.green_even
    }
}

/// RGB to YCbCr coefficients: luma in Q8 (non-negative, summing to at most 256) plus an
/// offset, chroma in Q7 (each sign's coefficients summing to at most 128 in magnitude).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct YuvCoeffs {
    pub y: [u16; 3],
    pub y_offset: u16,
    pub u: [i16; 3],
    pub v: [i16; 3],
}

impl YuvCoeffs {
    /// BT.601 full range (JFIF / sYCC).
    pub const BT601_FULL: Self = Self {
        y: [77, 150, 29],
        y_offset: 0,
        u: [-22, -42, 64],
        v: [64, -54, -10],
    };
    /// BT.709 limited range (16..235 luma, 16..240 chroma).
    pub const BT709_LIMITED: Self = Self {
        y: [47, 157, 16],
        y_offset: 16,
        u: [-13, -43, 56],
        v: [56, -51, -5],
    };
}

/// The first compiled-in backend leaf's result: `Some((backend, pixels done))` when it handled
/// a prefix of the row, `None` when no leaf applies.
macro_rules! leaf {
    ($name:ident($($arg:expr),* $(,)?)) => {{
        #[allow(unused_mut, unused_assignments)]
        let mut outcome: Option<(SimdBackend, usize)> = None;
        #[cfg(all(feature = "neon", target_arch = "aarch64"))]
        {
            outcome = neon::$name($($arg),*);
        }
        #[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            outcome = x86::$name(X86FeatureSet::detect(), $($arg),*);
        }
        outcome
    }};
}

/// Finish the pixels after the leaf's prefix with the scalar kernel.
fn finish(
    outcome: Option<(SimdBackend, usize)>,
    width: usize,
    scalar_tail: impl FnOnce(usize, usize),
) -> SimdBackend {
    let (backend, done) = outcome.unwrap_or((SimdBackend::Scalar, 0));
    if done < width {
        scalar_tail(done, width - done);
    }
    backend
}

/// Bytes of a CSI-2 packed RAW10 row of `width` pixels.
pub const fn raw10_bytes(width: usize) -> usize {
    width.div_ceil(4) * 5
}

/// Bytes of a CSI-2 packed RAW12 row of `width` pixels.
pub const fn raw12_bytes(width: usize) -> usize {
    width.div_ceil(2) * 3
}

/// See [`scalar::unpack_raw10_row`]. `src` holds at least [`raw10_bytes`]`(width)` bytes.
pub fn unpack_raw10_row(src: &[u8], dst: &mut [u16], width: usize) -> SimdBackend {
    let (src, dst) = (&src[..raw10_bytes(width)], &mut dst[..width]);
    let outcome = leaf!(unpack_raw10_row(src, dst, width));
    finish(outcome, width, |d, n| {
        debug_assert_eq!(d % 4, 0);
        scalar::unpack_raw10_row(&src[d / 4 * 5..], &mut dst[d..], n)
    })
}

/// See [`scalar::unpack_raw12_row`]. `src` holds at least [`raw12_bytes`]`(width)` bytes.
pub fn unpack_raw12_row(src: &[u8], dst: &mut [u16], width: usize) -> SimdBackend {
    let (src, dst) = (&src[..raw12_bytes(width)], &mut dst[..width]);
    let outcome = leaf!(unpack_raw12_row(src, dst, width));
    finish(outcome, width, |d, n| {
        debug_assert_eq!(d % 2, 0);
        scalar::unpack_raw12_row(&src[d / 2 * 3..], &mut dst[d..], n)
    })
}

/// See [`scalar::front_row`].
pub fn front_row(
    row: &mut [u16],
    black: [u16; 2],
    gains: &[u16],
    shift: u32,
    width: usize,
) -> SimdBackend {
    let (row, gains) = (&mut row[..width], &gains[..width]);
    let outcome = leaf!(front_row(row, black, gains, shift, width));
    finish(outcome, width, |d, n| {
        // Leaves stop on even columns, so the black-level parity is unchanged.
        debug_assert_eq!(d % 2, 0);
        scalar::front_row(&mut row[d..], black, &gains[d..], shift, n)
    })
}

/// See [`scalar::demosaic_bilinear_row`].
pub fn demosaic_bilinear_row(
    rows: [&[u16]; 3],
    out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> SimdBackend {
    let rows = rows.map(|r| &r[..width + 2]);
    let [r, g, b] = out;
    let (r, g, b) = (&mut r[..width], &mut g[..width], &mut b[..width]);
    let outcome = leaf!(demosaic_bilinear_row(
        rows,
        [&mut *r, &mut *g, &mut *b],
        width,
        kind
    ));
    finish(outcome, width, |d, n| {
        debug_assert_eq!(d % 2, 0);
        scalar::demosaic_bilinear_row(
            rows.map(|row| &row[d..]),
            [&mut r[d..], &mut g[d..], &mut b[d..]],
            n,
            kind,
        )
    })
}

/// See [`scalar::demosaic_mhc_row`].
pub fn demosaic_mhc_row(
    rows: [&[u16]; 5],
    out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> SimdBackend {
    let rows = rows.map(|r| &r[..width + 4]);
    let [r, g, b] = out;
    let (r, g, b) = (&mut r[..width], &mut g[..width], &mut b[..width]);
    let outcome = leaf!(demosaic_mhc_row(
        rows,
        [&mut *r, &mut *g, &mut *b],
        width,
        kind
    ));
    finish(outcome, width, |d, n| {
        debug_assert_eq!(d % 2, 0);
        scalar::demosaic_mhc_row(
            rows.map(|row| &row[d..]),
            [&mut r[d..], &mut g[d..], &mut b[d..]],
            n,
            kind,
        )
    })
}

/// See [`scalar::bayer_luma_row`].
pub fn bayer_luma_row(rows: [&[u16]; 3], dst: &mut [u16], width: usize) -> SimdBackend {
    let rows = rows.map(|r| &r[..width + 2]);
    let dst = &mut dst[..width];
    let outcome = leaf!(bayer_luma_row(rows, dst, width));
    finish(outcome, width, |d, n| {
        scalar::bayer_luma_row(rows.map(|row| &row[d..]), &mut dst[d..], n)
    })
}

/// See [`scalar::quad_rgb_row`]; `width` is the output (half) width.
pub fn quad_rgb_row(
    top: &[u16],
    bottom: &[u16],
    out: [&mut [u16]; 3],
    width: usize,
    pattern: CfaPattern,
) -> SimdBackend {
    let (top, bottom) = (&top[..2 * width], &bottom[..2 * width]);
    let [r, g, b] = out;
    let (r, g, b) = (&mut r[..width], &mut g[..width], &mut b[..width]);
    let outcome = leaf!(quad_rgb_row(
        top,
        bottom,
        [&mut *r, &mut *g, &mut *b],
        width,
        pattern
    ));
    finish(outcome, width, |d, n| {
        scalar::quad_rgb_row(
            &top[2 * d..],
            &bottom[2 * d..],
            [&mut r[d..], &mut g[d..], &mut b[d..]],
            n,
            pattern,
        )
    })
}

/// See [`scalar::quad_luma_row`]; `width` is the output (half) width.
pub fn quad_luma_row(top: &[u16], bottom: &[u16], dst: &mut [u16], width: usize) -> SimdBackend {
    let (top, bottom, dst) = (&top[..2 * width], &bottom[..2 * width], &mut dst[..width]);
    let outcome = leaf!(quad_luma_row(top, bottom, dst, width));
    finish(outcome, width, |d, n| {
        scalar::quad_luma_row(&top[2 * d..], &bottom[2 * d..], &mut dst[d..], n)
    })
}

/// See [`scalar::ccm_row`].
pub fn ccm_row(planes: [&mut [u16]; 3], m: &[i16; 9], width: usize) -> SimdBackend {
    let [r, g, b] = planes;
    let (r, g, b) = (&mut r[..width], &mut g[..width], &mut b[..width]);
    let outcome = leaf!(ccm_row([&mut *r, &mut *g, &mut *b], m, width));
    finish(outcome, width, |d, n| {
        scalar::ccm_row([&mut r[d..], &mut g[d..], &mut b[d..]], m, n)
    })
}

/// See [`scalar::narrow_row`].
pub fn narrow_row(src: &[u16], dst: &mut [u8], width: usize) -> SimdBackend {
    let (src, dst) = (&src[..width], &mut dst[..width]);
    let outcome = leaf!(narrow_row(src, dst, width));
    finish(outcome, width, |d, n| {
        scalar::narrow_row(&src[d..], &mut dst[d..], n)
    })
}

/// A tone curve from 12-bit working values to 8 bits: 257 nodes 16 input codes apart,
/// interpolated linearly, `out(x) = (n[x >> 4] (16 - f) + n[(x >> 4) + 1] f + 8) >> 4` with
/// `f = x & 15` (inputs above 4095 clamp), read from the expanded 4096-entry table.
///
/// There is no vector leaf: NEON `tbl` over 256-byte tables (eight 4-register lookups per 16
/// pixels) measured slower on the Cortex-A76 than scalar loads from the 4 KiB table.
#[derive(Clone, PartialEq, Eq)]
pub struct ToneLut {
    nodes: [u8; 257],
    full: Box<[u8; 4096]>,
}

impl std::fmt::Debug for ToneLut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToneLut")
            .field("nodes", &&self.nodes[..])
            .finish()
    }
}

impl ToneLut {
    pub fn from_nodes(nodes: [u8; 257]) -> Self {
        let mut full = Box::new([0u8; 4096]);
        for (x, v) in full.iter_mut().enumerate() {
            let (i, f) = (x >> 4, (x & 15) as u32);
            *v = ((nodes[i] as u32 * (16 - f) + nodes[i + 1] as u32 * f + 8) >> 4) as u8;
        }
        Self { nodes, full }
    }

    /// Nodes sampled from `curve` (0..1 to 0..1) at inputs `16 k / 4095`.
    pub fn from_curve(curve: impl Fn(f32) -> f32) -> Self {
        let nodes = std::array::from_fn(|k| {
            let x = (k as f32 * 16.0 / scalar::WORK_MAX as f32).min(1.0);
            (curve(x) * 255.0).round().clamp(0.0, 255.0) as u8
        });
        Self::from_nodes(nodes)
    }

    pub fn nodes(&self) -> &[u8; 257] {
        &self.nodes
    }

    pub fn full(&self) -> &[u8; 4096] {
        &self.full
    }
}

/// See [`scalar::lut_row`] and [`ToneLut`] (scalar always).
pub fn lut_row(src: &[u16], dst: &mut [u8], lut: &ToneLut, width: usize) -> SimdBackend {
    scalar::lut_row(src, dst, lut.full(), width);
    SimdBackend::Scalar
}

/// See [`scalar::interleave_rgb_row`].
pub fn interleave_rgb_row(planes: [&[u8]; 3], dst: &mut [u8], width: usize) -> SimdBackend {
    let planes = planes.map(|p| &p[..width]);
    let dst = &mut dst[..width * 3];
    let outcome = leaf!(interleave_rgb_row(planes, dst, width));
    finish(outcome, width, |d, n| {
        scalar::interleave_rgb_row(planes.map(|p| &p[d..]), &mut dst[d * 3..], n)
    })
}

/// See [`scalar::rgb_to_y_row`].
pub fn rgb_to_y_row(
    planes: [&[u8]; 3],
    dst: &mut [u8],
    width: usize,
    c: &YuvCoeffs,
) -> SimdBackend {
    let planes = planes.map(|p| &p[..width]);
    let dst = &mut dst[..width];
    let outcome = leaf!(rgb_to_y_row(planes, dst, width, c));
    finish(outcome, width, |d, n| {
        scalar::rgb_to_y_row(planes.map(|p| &p[d..]), &mut dst[d..], n, c)
    })
}

/// See [`scalar::rgb_to_uv_row`]; `width` is the chroma (half) width.
pub fn rgb_to_uv_row(
    top: [&[u8]; 3],
    bottom: [&[u8]; 3],
    u: &mut [u8],
    v: &mut [u8],
    width: usize,
    c: &YuvCoeffs,
    interleaved: bool,
) -> SimdBackend {
    let (top, bottom) = (
        top.map(|p| &p[..2 * width]),
        bottom.map(|p| &p[..2 * width]),
    );
    let (u, v) = if interleaved {
        (&mut u[..2 * width], &mut v[..0])
    } else {
        (&mut u[..width], &mut v[..width])
    };
    let outcome = leaf!(rgb_to_uv_row(
        top,
        bottom,
        &mut *u,
        &mut *v,
        width,
        c,
        interleaved
    ));
    finish(outcome, width, |d, n| {
        let (uo, vo) = if interleaved { (2 * d, 0) } else { (d, d) };
        scalar::rgb_to_uv_row(
            top.map(|p| &p[2 * d..]),
            bottom.map(|p| &p[2 * d..]),
            &mut u[uo..],
            &mut v[vo..],
            n,
            c,
            interleaved,
        )
    })
}
