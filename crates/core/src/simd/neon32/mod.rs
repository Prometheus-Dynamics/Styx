//! 32-bit Arm NEON leaves (ARMv7-A, e.g. the RV1106's Cortex-A7), for CPUs whose kernel
//! reports NEON (`AT_HWCAP`). Inline assembly that enables NEON itself (Rust's 32-bit Arm NEON
//! intrinsics and target features are unstable), so the same binary runs on cores without it,
//! on the scalar kernels. Each must produce exactly the scalar result; each returns the pixels
//! it handled (whole blocks) and the dispatcher finishes the row.

pub(super) mod rows;
pub(super) mod transform;

use std::sync::OnceLock;

use super::{ColorLayout, SimdBackend, TransposeTile};

/// `HWCAP_NEON` on 32-bit Arm Linux.
const HWCAP_NEON: libc::c_ulong = 1 << 12;

/// Whether the CPU has NEON, asked once.
pub(super) fn available() -> bool {
    static HAS_NEON: OnceLock<bool> = OnceLock::new();
    // SAFETY: getauxval only reads the process's auxiliary vector.
    *HAS_NEON.get_or_init(|| unsafe { libc::getauxval(libc::AT_HWCAP) } & HWCAP_NEON != 0)
}

/// Whole blocks of `block` pixels in `width`, when NEON is there and there is one.
fn steps(width: usize, block: usize) -> Option<usize> {
    let steps = width / block;
    (steps > 0 && available()).then_some(steps)
}

/// Run `leaf` on the row's whole blocks of `block` pixels.
fn blocks(width: usize, block: usize, leaf: impl FnOnce(usize)) -> Option<(SimdBackend, usize)> {
    let steps = steps(width, block)?;
    leaf(steps);
    Some((SimdBackend::Neon, steps * block))
}

// SAFETY (every leaf below): `blocks` runs it only with NEON, for `steps * block <= width`
// pixels, and the dispatcher sized both rows for `width`.

pub(super) fn swap_rb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 16, |n| unsafe { rows::swap_rb24(src, dst, n) })
}

pub(super) fn swap_rb32_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 16, |n| unsafe { rows::swap_rb32(src, dst, n) })
}

pub(super) fn x32_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    swap: bool,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 16, |n| unsafe {
        rows::x32_to_rgb24(src, dst, n, swap)
    })
}

pub(super) fn rgb_to_luma_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    layout: ColorLayout,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe {
        rows::rgb_to_luma(src, dst, n, layout)
    })
}

pub(super) fn rgb24_to_rgba_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 16, |n| unsafe { rows::rgb24_to_rgba(src, dst, n) })
}

pub(super) fn gray8_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 16, |n| unsafe { rows::gray8_to_rgb24(src, dst, n) })
}

pub(super) fn gray16le_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 16, |n| unsafe {
        rows::gray16le_to_rgb24(src, dst, n)
    })
}

pub(super) fn rgb48le_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    swap: bool,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 8, |n| unsafe {
        rows::rgb48le_to_rgb24(src, dst, n, swap)
    })
}

pub(super) fn yuyv_luma_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 16, |n| unsafe { rows::yuyv_luma(src, dst, n) })
}

/// 2x2 box average, 16 outputs a step.
pub(super) fn box2_row(
    top: &[u8],
    bottom: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    blocks(width, 16, |n| unsafe { rows::box2(top, bottom, dst, n) })
}

/// Source pixels `0..done` reversed into the end of `dst`.
pub(super) fn reverse_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    bpp: usize,
) -> Option<(SimdBackend, core::ops::Range<usize>)> {
    let block = if bpp == 3 { 8 } else { 16 / bpp };
    let steps = steps(width, block)?;
    let last = (width - block) * bpp;
    // SAFETY: NEON is there; the blocks read `steps * block` source pixels and write them to
    // the output pixels `width - steps * block..width`, backwards from the last block.
    unsafe { rows::reverse(src, dst[last..].as_mut_ptr(), steps, bpp) };
    Some((SimdBackend::Neon, 0..steps * block))
}

pub(super) fn transpose_tile(bpp: usize) -> Option<(SimdBackend, TransposeTile)> {
    if !available() {
        return None;
    }
    let tile: TransposeTile = match bpp {
        1 => transform::transpose_tile_u8,
        2 => transform::transpose_tile_u16,
        3 => transform::transpose_tile_rgb,
        4 => transform::transpose_tile_u32,
        _ => return None,
    };
    Some((SimdBackend::Neon, tile))
}
