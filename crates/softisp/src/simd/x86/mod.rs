//! x86 leaves, chosen per call from the given CPU features (AVX2, then SSSE3 or SSE2). The
//! dispatchers pass the detected features; tests pass restricted sets to reach every leaf. Each
//! returns the pixels it handled; the dispatcher finishes the row with the scalar kernel.

mod bayer;
mod color;
mod vec;

use super::{RowKind, SimdBackend, ToneLut, X86FeatureSet, YuvCoeffs};
use crate::format::CfaPattern;

fn done(backend: SimdBackend, pixels: usize) -> Option<(SimdBackend, usize)> {
    (pixels > 0).then_some((backend, pixels))
}

/// Run the AVX2 leaf when available and the row holds at least `avx_min` pixels, else the
/// SSE2 one.
macro_rules! pick {
    ($f:expr, $width:expr, $avx_min:expr, $avx:expr, $sse:expr) => {{
        // SAFETY (every leaf): the feature it needs was detected; the dispatcher sized the
        // slices for `width` pixels.
        if $f.avx2 && $width >= $avx_min {
            done(SimdBackend::X86Avx2, unsafe { $avx })
        } else if $f.sse2 {
            done(SimdBackend::X86Sse2, unsafe { $sse })
        } else {
            None
        }
    }};
}

fn unpack(
    f: X86FeatureSet,
    src: &[u8],
    dst: &mut [u16],
    width: usize,
    raw12: bool,
) -> Option<(SimdBackend, usize)> {
    if f.avx2 && width >= 16 {
        // SAFETY: AVX2 detected; the leaf bounds-checks its loads against `src`.
        return done(SimdBackend::X86Avx2, unsafe {
            color::unpack_avx2(src, dst, width, raw12)
        });
    }
    if f.ssse3 {
        // SAFETY: SSSE3 detected; as above.
        return done(SimdBackend::X86Ssse3, unsafe {
            color::unpack_ssse3(src, dst, width, raw12)
        });
    }
    None
}

pub(super) fn unpack_raw10_row(
    f: X86FeatureSet,
    src: &[u8],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    unpack(f, src, dst, width, false)
}

pub(super) fn unpack_raw12_row(
    f: X86FeatureSet,
    src: &[u8],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    unpack(f, src, dst, width, true)
}

pub(super) fn front_row(
    f: X86FeatureSet,
    row: &mut [u16],
    black: [u16; 2],
    gains: &[u16],
    shift: u32,
    width: usize,
) -> Option<(SimdBackend, usize)> {
    pick!(
        f,
        width,
        16,
        bayer::front_avx2(row, black, gains, shift, width),
        bayer::front_sse2(row, black, gains, shift, width)
    )
}

pub(super) fn demosaic_bilinear_row(
    f: X86FeatureSet,
    rows: [&[u16]; 3],
    out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> Option<(SimdBackend, usize)> {
    pick!(
        f,
        width,
        16,
        bayer::demosaic_bilinear_avx2(rows, out, width, kind),
        bayer::demosaic_bilinear_sse2(rows, out, width, kind)
    )
}

pub(super) fn demosaic_mhc_row(
    f: X86FeatureSet,
    rows: [&[u16]; 5],
    out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> Option<(SimdBackend, usize)> {
    pick!(
        f,
        width,
        16,
        bayer::demosaic_mhc_avx2(rows, out, width, kind),
        bayer::demosaic_mhc_sse2(rows, out, width, kind)
    )
}

pub(super) fn bayer_luma_row(
    f: X86FeatureSet,
    rows: [&[u16]; 3],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    pick!(
        f,
        width,
        16,
        bayer::bayer_luma_avx2(rows, dst, width),
        bayer::bayer_luma_sse2(rows, dst, width)
    )
}

pub(super) fn quad_rgb_row(
    f: X86FeatureSet,
    top: &[u16],
    bottom: &[u16],
    out: [&mut [u16]; 3],
    width: usize,
    pattern: CfaPattern,
) -> Option<(SimdBackend, usize)> {
    pick!(
        f,
        width,
        16,
        bayer::quad_rgb_avx2(top, bottom, out, width, pattern),
        bayer::quad_rgb_sse2(top, bottom, out, width, pattern)
    )
}

pub(super) fn quad_luma_row(
    f: X86FeatureSet,
    top: &[u16],
    bottom: &[u16],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    pick!(
        f,
        width,
        16,
        bayer::quad_luma_avx2(top, bottom, dst, width),
        bayer::quad_luma_sse2(top, bottom, dst, width)
    )
}

pub(super) fn ccm_row(
    f: X86FeatureSet,
    planes: [&mut [u16]; 3],
    m: &[i16; 9],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    pick!(
        f,
        width,
        16,
        color::ccm_avx2(planes, m, width),
        color::ccm_sse2(planes, m, width)
    )
}

pub(super) fn narrow_row(
    f: X86FeatureSet,
    src: &[u16],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    pick!(
        f,
        width,
        32,
        color::narrow_avx2(src, dst, width),
        color::narrow_sse2(src, dst, width)
    )
}

pub(super) fn interleave_rgb_row(
    f: X86FeatureSet,
    planes: [&[u8]; 3],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    // SAFETY: SSSE3 detected; the dispatcher sized the slices.
    f.ssse3
        .then(|| {
            done(SimdBackend::X86Ssse3, unsafe {
                color::interleave_rgb_ssse3(planes, dst, width)
            })
        })
        .flatten()
}

pub(super) fn rgb_to_y_row(
    f: X86FeatureSet,
    planes: [&[u8]; 3],
    dst: &mut [u8],
    width: usize,
    c: &YuvCoeffs,
) -> Option<(SimdBackend, usize)> {
    pick!(
        f,
        width,
        32,
        color::rgb_to_y_avx2(planes, dst, width, c),
        color::rgb_to_y_sse2(planes, dst, width, c)
    )
}

// Many borrowed rows, as the scalar oracle.
#[allow(clippy::too_many_arguments)]
pub(super) fn rgb_to_uv_row(
    f: X86FeatureSet,
    top: [&[u8]; 3],
    bottom: [&[u8]; 3],
    u: &mut [u8],
    v: &mut [u8],
    width: usize,
    c: &YuvCoeffs,
    interleaved: bool,
) -> Option<(SimdBackend, usize)> {
    pick!(
        f,
        width,
        16,
        color::rgb_to_uv_avx2(top, bottom, u, v, width, c, interleaved),
        color::rgb_to_uv_sse2(top, bottom, u, v, width, c, interleaved)
    )
}

/// No x86 leaf: a 256-entry byte table needs 16 `pshufb` per lookup, no faster than the
/// scalar 4096-entry table.
pub(super) fn lut_row(
    _f: X86FeatureSet,
    _src: &[u16],
    _dst: &mut [u8],
    _lut: &ToneLut,
    _width: usize,
) -> Option<(SimdBackend, usize)> {
    None
}
