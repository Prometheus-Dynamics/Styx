//! AArch64 NEON leaves (NEON is part of the AArch64 baseline). Each returns the pixels it
//! handled; the dispatcher finishes the row with the scalar kernel.
//!
//! SAFETY (every leaf call below): NEON is always available on AArch64, and the dispatcher
//! sized the slices for `width` pixels as each leaf documents.

mod bayer;
mod color;

use super::{RowKind, SimdBackend, YuvCoeffs};
use crate::format::CfaPattern;

fn done(pixels: usize) -> Option<(SimdBackend, usize)> {
    (pixels > 0).then_some((SimdBackend::Neon, pixels))
}

pub(super) fn unpack_raw10_row(
    src: &[u8],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { bayer::unpack(src, dst, width, false) })
}

pub(super) fn unpack_raw12_row(
    src: &[u8],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { bayer::unpack(src, dst, width, true) })
}

pub(super) fn front_row(
    row: &mut [u16],
    black: [u16; 2],
    gains: &[u16],
    shift: u32,
    width: usize,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { bayer::front(row, black, gains, shift, width) })
}

pub(super) fn demosaic_bilinear_row(
    rows: [&[u16]; 3],
    out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { bayer::demosaic_bilinear(rows, out, width, kind) })
}

pub(super) fn demosaic_mhc_row(
    rows: [&[u16]; 5],
    out: [&mut [u16]; 3],
    width: usize,
    kind: RowKind,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { bayer::demosaic_mhc(rows, out, width, kind) })
}

pub(super) fn bayer_luma_row(
    rows: [&[u16]; 3],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { bayer::bayer_luma(rows, dst, width) })
}

pub(super) fn quad_rgb_row(
    top: &[u16],
    bottom: &[u16],
    out: [&mut [u16]; 3],
    width: usize,
    pattern: CfaPattern,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { bayer::quad_rgb(top, bottom, out, width, pattern) })
}

pub(super) fn quad_luma_row(
    top: &[u16],
    bottom: &[u16],
    dst: &mut [u16],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { bayer::quad_luma(top, bottom, dst, width) })
}

pub(super) fn ccm_row(
    planes: [&mut [u16]; 3],
    m: &[i16; 9],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { color::ccm(planes, m, width) })
}

pub(super) fn narrow_row(
    src: &[u16],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { color::narrow(src, dst, width) })
}

pub(super) fn interleave_rgb_row(
    planes: [&[u8]; 3],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { color::interleave_rgb(planes, dst, width) })
}

pub(super) fn rgb_to_y_row(
    planes: [&[u8]; 3],
    dst: &mut [u8],
    width: usize,
    c: &YuvCoeffs,
) -> Option<(SimdBackend, usize)> {
    done(unsafe { color::rgb_to_y(planes, dst, width, c) })
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
    done(unsafe { color::rgb_to_uv(top, bottom, u, v, width, c, interleaved) })
}

pub(super) fn zone_sums(
    rgb: [&[u16]; 3],
    width: usize,
    sat: u16,
) -> Option<(SimdBackend, usize, [u32; 5])> {
    let (done, sums) = unsafe { bayer::zone_sums(rgb, width, sat) };
    (done > 0).then_some((SimdBackend::Neon, done, sums))
}
