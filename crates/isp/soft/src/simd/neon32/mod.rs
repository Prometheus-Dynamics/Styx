//! 32-bit Arm NEON leaves of the software ISP (ARMv7-A, e.g. the RV1106's Cortex-A7), for CPUs
//! whose kernel reports NEON (`styx_simd::has_neon32`). Inline assembly that enables NEON
//! itself (Rust's 32-bit Arm NEON intrinsics are unstable), so the same binary runs on cores
//! without it, on the scalar kernels. Each runs whole blocks from the start of the row, must
//! produce exactly the scalar result and returns the pixels it handled.

mod bayer;
mod color;

use super::{RowKind, SimdBackend, YuvCoeffs};
use crate::format::CfaPattern;

/// One loop of assembly: `pre` once, then `body` `{n}` times. `d0`-`d27` are clobbered; the
/// callers pass constants in `d28`-`d31` (or name those they clobber).
macro_rules! neon {
    ([$($pre:expr),* $(,)?], [$($body:expr),* $(,)?], [$($post:expr),* $(,)?], $($operands:tt)*) => {
        core::arch::asm!(
            ".fpu neon",
            $($pre,)*
            "2:",
            $($body,)*
            "subs {n}, {n}, #1",
            "bne 2b",
            $($post,)*
            $($operands)*,
            out("d0") _, out("d1") _, out("d2") _, out("d3") _, out("d4") _, out("d5") _,
            out("d6") _, out("d7") _, out("d8") _, out("d9") _, out("d10") _, out("d11") _,
            out("d12") _, out("d13") _, out("d14") _, out("d15") _, out("d16") _,
            out("d17") _, out("d18") _, out("d19") _, out("d20") _, out("d21") _,
            out("d22") _, out("d23") _, out("d24") _, out("d25") _, out("d26") _,
            out("d27") _,
            options(nostack),
        )
    };
}
use neon;
mod post;
pub(super) use post::*;

/// A 64-bit NEON register of four 16-bit lanes, as the `f64` an asm operand takes.
fn lanes16(l: [u16; 4]) -> f64 {
    f64::from_bits(l.iter().rev().fold(0, |w, &v| w << 16 | u64::from(v)))
}

/// Run `leaf` on the row's whole blocks of `block` pixels, when the CPU has NEON.
fn blocks(width: usize, block: usize, leaf: impl FnOnce(usize)) -> Option<(SimdBackend, usize)> {
    let steps = width / block;
    if steps == 0 || !styx_simd::has_neon32() {
        return None;
    }
    leaf(steps);
    Some((SimdBackend::Neon, steps * block))
}

// SAFETY (every leaf below): `blocks` runs it only with NEON, for `steps * block <= width`
// pixels, and the dispatcher sized every row for `width` as the scalar kernel documents.

pub(super) fn unpack_raw10_row(
    src: &[u8],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe { bayer::unpack(src, dst, n, false) })
}

pub(super) fn unpack_raw12_row(
    src: &[u8],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe { bayer::unpack(src, dst, n, true) })
}

pub(super) fn front_row(
    row: &mut [u16],
    black: [u16; 2],
    gains: &[u16],
    shift: u32,
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe {
        bayer::front(row, black, gains, shift, n)
    })
}

pub(super) fn demosaic_bilinear_row(
    rows: [&[u16]; 3],
    out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe {
        bayer::demosaic_bilinear(rows, out, n, kind)
    })
}

pub(super) fn demosaic_mhc_row(
    rows: [&[u16]; 5],
    out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe {
        bayer::demosaic_mhc(rows, out, n, kind)
    })
}

pub(super) fn demosaic_edge_row(
    rows: [&[u16]; 5],
    out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe {
        bayer::demosaic_edge(rows, out, n, kind)
    })
}

pub(super) fn bayer_luma_row(
    rows: [&[u16]; 3],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe { bayer::bayer_luma(rows, dst, n) })
}

pub(super) fn quad_rgb_row(
    top: &[u16],
    bottom: &[u16],
    out: [&mut [u16]; 3],
    width: usize,
    pattern: CfaPattern,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe {
        bayer::quad_rgb(top, bottom, out, n, pattern)
    })
}

pub(super) fn quad_luma_row(
    top: &[u16],
    bottom: &[u16],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe {
        bayer::quad_luma(top, bottom, dst, n)
    })
}

pub(super) fn ccm_row(
    planes: [&mut [u16]; 3],
    m: &[i16; 9],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe { color::ccm(planes, m, n) })
}

pub(super) fn narrow_row(
    src: &[u16],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 16, |n| unsafe { color::narrow(src, dst, n) })
}

pub(super) fn lut_row(
    src: &[u16],
    dst: &mut [u8],
    lut: &super::ToneLut,
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe { color::lut(src, dst, lut.full(), n) })
}

pub(super) fn poly_row(
    src: &[u16],
    dst: &mut [u8],
    poly: &super::poly::PolyTone,
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe { color::poly(src, dst, &poly.c, n) })
}

pub(super) fn interleave_rgb_row(
    planes: [&[u8]; 3],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 16, |n| unsafe {
        color::interleave_rgb(planes, dst, n)
    })
}

pub(super) fn rgb_to_y_row(
    planes: [&[u8]; 3],
    dst: &mut [u8],
    width: usize,
    c: &YuvCoeffs,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 16, |n| unsafe { color::rgb_to_y(planes, dst, n, c) })
}

// Many borrowed rows, as the scalar oracle.
#[allow(clippy::too_many_arguments)]
pub(super) fn rgb_to_uv_row(
    top: [&[u8]; 3],
    bottom: [&[u8]; 3],
    u: &mut [u8],
    v: &mut [u8],
    width: usize,
    c: &YuvCoeffs,
    interleaved: bool,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe {
        color::rgb_to_uv(top, bottom, u, v, n, c, interleaved)
    })
}

pub(super) fn zone_sums(
    rgb: [&[u16]; 3],
    width: usize,
    sat: u16,
) -> Option<(SimdBackend, usize, [u32; 5])> {
    let mut sums = [0; 5];
    blocks(width, 8, |n| {
        sums = unsafe { bayer::zone_sums(rgb, n, sat) }
    })
    .map(|(backend, done)| (backend, done, sums))
}

pub(super) fn luma_bins_row(
    rgb: [&[u16]; 3],
    dst: &mut [u16],
    bins: u16,
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe { bayer::luma_bins(rgb, dst, bins, n) })
}
