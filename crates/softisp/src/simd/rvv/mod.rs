//! RISC-V vector (RVV 1.0) leaves of the software ISP, for CPUs whose kernel reports the V
//! extension (the K230D's C908; `styx_core::simd::has_rvv`). Inline assembly that enables the
//! extension itself, so the same binary runs on cores without it, on the scalar kernels. Each
//! covers the whole row (the hardware picks the strip length) and must produce exactly the
//! scalar result.

mod bayer;
mod color;

use super::{RowKind, SimdBackend, YuvCoeffs};
use crate::format::CfaPattern;

/// One strip of assembly: `body` sets `{n}` with its `vsetvli` (the strip's pixels) and the
/// value is `n`. Every vector register is clobbered.
macro_rules! rvv {
    ([$($body:expr),* $(,)?], $($operands:tt)*) => {{
        let n: usize;
        core::arch::asm!(
            ".option push",
            ".option arch, +v",
            $($body,)*
            ".option pop",
            n = out(reg) n,
            $($operands)*,
            out("v0") _, out("v1") _, out("v2") _, out("v3") _, out("v4") _, out("v5") _,
            out("v6") _, out("v7") _, out("v8") _, out("v9") _, out("v10") _, out("v11") _,
            out("v12") _, out("v13") _, out("v14") _, out("v15") _, out("v16") _,
            out("v17") _, out("v18") _, out("v19") _, out("v20") _, out("v21") _,
            out("v22") _, out("v23") _, out("v24") _, out("v25") _, out("v26") _,
            out("v27") _, out("v28") _, out("v29") _, out("v30") _, out("v31") _,
            options(nostack),
        );
        n
    }};
}
use rvv;
mod post;
pub(super) use post::*;

/// Strips over `units`: `strip(done, left)` does some of the `left` units after the first
/// `done` and returns how many.
fn strips(units: usize, mut strip: impl FnMut(usize, usize) -> usize) {
    let mut done = 0;
    while done < units {
        done += strip(done, units - done);
    }
}

/// `Some((Rvv, done))` when the CPU has vectors and there is something to do.
fn run(done: usize, leaf: impl FnOnce()) -> Option<(SimdBackend, usize)> {
    if done == 0 || !styx_core::simd::has_rvv() {
        return None;
    }
    leaf();
    Some((SimdBackend::Rvv, done))
}

// SAFETY (every leaf below): `run` calls it only with the V extension, for `done` pixels the
// dispatcher sized every row for.

pub(super) fn unpack_raw10_row(
    src: &[u8],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    run(width / 4 * 4, || unsafe {
        bayer::unpack_raw10(src, dst, width / 4)
    })
}

pub(super) fn unpack_raw12_row(
    src: &[u8],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    run(width / 2 * 2, || unsafe {
        bayer::unpack_raw12(src, dst, width / 2)
    })
}

pub(super) fn front_row(
    row: &mut [u16],
    black: [u16; 2],
    gains: &[u16],
    shift: u32,
    width: usize,
) -> Option<(SimdBackend, usize)> {
    run(width / 2 * 2, || unsafe {
        bayer::front(row, black, gains, shift, width / 2)
    })
}

pub(super) fn demosaic_bilinear_row(
    rows: [&[u16]; 3],
    out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> Option<(SimdBackend, usize)> {
    run(width, || unsafe {
        bayer::demosaic_bilinear(rows, out, width, kind)
    })
}

pub(super) fn demosaic_mhc_row(
    rows: [&[u16]; 5],
    out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> Option<(SimdBackend, usize)> {
    run(width, || unsafe {
        bayer::demosaic_mhc(rows, out, width, kind)
    })
}

pub(super) fn bayer_luma_row(
    rows: [&[u16]; 3],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    run(width, || unsafe { bayer::bayer_luma(rows, dst, width) })
}

pub(super) fn quad_rgb_row(
    top: &[u16],
    bottom: &[u16],
    out: [&mut [u16]; 3],
    width: usize,
    pattern: CfaPattern,
) -> Option<(SimdBackend, usize)> {
    run(width, || unsafe {
        bayer::quad_rgb(top, bottom, out, width, pattern)
    })
}

pub(super) fn quad_luma_row(
    top: &[u16],
    bottom: &[u16],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    run(width, || unsafe {
        bayer::quad_luma(top, bottom, dst, width)
    })
}

pub(super) fn ccm_row(
    planes: [&mut [u16]; 3],
    m: &[i16; 9],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    run(width, || unsafe { color::ccm(planes, m, width) })
}

pub(super) fn narrow_row(
    src: &[u16],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    run(width, || unsafe { color::narrow(src, dst, width) })
}

pub(super) fn lut_row(
    src: &[u16],
    dst: &mut [u8],
    lut: &super::ToneLut,
    width: usize,
) -> Option<(SimdBackend, usize)> {
    run(width, || unsafe { color::lut(src, dst, lut.full(), width) })
}

pub(super) fn poly_row(
    src: &[u16],
    dst: &mut [u8],
    poly: &super::poly::PolyTone,
    width: usize,
) -> Option<(SimdBackend, usize)> {
    run(width, || unsafe { color::poly(src, dst, &poly.c, width) })
}

pub(super) fn interleave_rgb_row(
    planes: [&[u8]; 3],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    run(width, || unsafe {
        color::interleave_rgb(planes, dst, width)
    })
}

pub(super) fn rgb_to_y_row(
    planes: [&[u8]; 3],
    dst: &mut [u8],
    width: usize,
    c: &YuvCoeffs,
) -> Option<(SimdBackend, usize)> {
    run(width, || unsafe { color::rgb_to_y(planes, dst, width, c) })
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
    run(width, || unsafe {
        color::rgb_to_uv(top, bottom, u, v, width, c, interleaved)
    })
}

pub(super) fn zone_sums(
    rgb: [&[u16]; 3],
    width: usize,
    sat: u16,
) -> Option<(SimdBackend, usize, [u32; 5])> {
    let mut sums = [0; 5];
    run(width, || {
        sums = unsafe { bayer::zone_sums(rgb, width, sat) }
    })
    .map(|(backend, done)| (backend, done, sums))
}

pub(super) fn luma_bins_row(
    rgb: [&[u16]; 3],
    dst: &mut [u16],
    bins: u16,
    width: usize,
) -> Option<(SimdBackend, usize)> {
    run(width, || unsafe { bayer::luma_bins(rgb, dst, bins, width) })
}
