//! AArch64 NEON leaves (NEON is part of the AArch64 baseline). Each returns the pixels it
//! handled; the dispatcher finishes the row with the scalar kernel.

pub(super) mod rows;
pub(super) mod transform;

use super::{SimdBackend, TransposeTile};

/// `Some((Neon, done))` for a leaf that handled `done` pixels, `None` when it handled none.
fn done(pixels: usize) -> Option<(SimdBackend, usize)> {
    (pixels > 0).then_some((SimdBackend::Neon, pixels))
}

pub(super) fn swap_rb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    // SAFETY: NEON is always available on AArch64; the dispatcher sized both rows for `width`.
    done(unsafe { rows::swap_rb24(src, dst, width) })
}

pub(super) fn swap_rb32_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    // SAFETY: as above.
    done(unsafe { rows::swap_rb32(src, dst, width) })
}

pub(super) fn x32_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    swap: bool,
) -> Option<(SimdBackend, usize)> {
    // SAFETY: as above.
    done(unsafe { rows::x32_to_rgb24(src, dst, width, swap) })
}

pub(super) fn rgb_to_luma_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    layout: super::ColorLayout,
) -> Option<(SimdBackend, usize)> {
    // SAFETY: as above.
    done(unsafe { rows::rgb_to_luma(src, dst, width, layout) })
}

pub(super) fn rgb24_to_rgba_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    // SAFETY: as above.
    done(unsafe { rows::rgb24_to_rgba(src, dst, width) })
}

pub(super) fn gray8_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    // SAFETY: as above.
    done(unsafe { rows::gray8_to_rgb24(src, dst, width) })
}

pub(super) fn gray16le_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    // SAFETY: as above.
    done(unsafe { rows::gray16le_to_rgb24(src, dst, width) })
}

pub(super) fn rgb48le_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    swap: bool,
) -> Option<(SimdBackend, usize)> {
    // SAFETY: as above.
    done(unsafe { rows::rgb48le_to_rgb24(src, dst, width, swap) })
}

pub(super) fn yuyv_luma_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    // SAFETY: as above.
    done(unsafe { rows::yuyv_luma(src, dst, width) })
}

pub(super) fn box2_row(
    top: &[u8],
    bottom: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    // SAFETY: as above.
    done(unsafe { rows::box2(top, bottom, dst, width) })
}

pub(super) fn reverse_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    bpp: usize,
) -> Option<(SimdBackend, core::ops::Range<usize>)> {
    // SAFETY: as above.
    let end = unsafe { rows::reverse(src, dst, width, bpp) };
    (end > 0).then_some((SimdBackend::Neon, 0..end))
}

pub(super) fn transpose_tile(bpp: usize) -> Option<(SimdBackend, TransposeTile)> {
    let tile: TransposeTile = match bpp {
        1 => transform::transpose_tile_u8,
        2 => transform::transpose_tile_u16,
        3 => transform::transpose_tile_rgb,
        4 => transform::transpose_tile_u32,
        _ => return None,
    };
    Some((SimdBackend::Neon, tile))
}
