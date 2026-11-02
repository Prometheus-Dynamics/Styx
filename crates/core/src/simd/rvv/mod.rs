//! RISC-V vector (RVV 1.0) leaves, for CPUs whose kernel reports the V extension (the
//! K230D's C908): the vector length is the hardware's, chosen per strip by `vsetvli`, so a
//! leaf covers whole rows. Inline assembly that enables the extension itself (Rust's RVV
//! intrinsics, `target_feature = "v"` and its detection are unstable), so the same binary
//! runs on cores without it, on the scalar kernels. Each must produce exactly the scalar
//! result.

pub(super) mod h264;
pub(super) mod jpeg;
pub(super) mod rows;
pub(super) mod transform;

use std::sync::OnceLock;

use super::{ColorLayout, SimdBackend, TransposeTile, YuvMatrix};

/// Whether the CPU has the V extension (`AT_HWCAP` bit `'V' - 'A'`), asked once.
pub(super) fn available() -> bool {
    static HAS_V: OnceLock<bool> = OnceLock::new();
    // SAFETY: getauxval only reads the process's auxiliary vector.
    *HAS_V.get_or_init(|| unsafe { libc::getauxval(libc::AT_HWCAP) } & (1 << (b'V' - b'A')) != 0)
}

/// Run `leaf` on the whole row when the CPU has vectors.
fn whole(width: usize, leaf: impl FnOnce()) -> Option<(SimdBackend, usize)> {
    if width == 0 || !available() {
        return None;
    }
    leaf();
    Some((SimdBackend::Rvv, width))
}

// SAFETY (every leaf below): `whole` runs it only with the V extension, and the dispatcher
// sized both rows for `width`.

pub(super) fn swap_rb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    whole(width, || unsafe { rows::swap_rb24(src, dst, width) })
}

pub(super) fn swap_rb32_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    whole(width, || unsafe { rows::swap_rb32(src, dst, width) })
}

pub(super) fn x32_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    swap: bool,
) -> Option<(SimdBackend, usize)> {
    whole(width, || unsafe {
        rows::x32_to_rgb24(src, dst, width, swap)
    })
}

pub(super) fn rgb_to_luma_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    layout: ColorLayout,
) -> Option<(SimdBackend, usize)> {
    whole(width, || unsafe {
        rows::rgb_to_luma(src, dst, width, layout)
    })
}

pub(super) fn rgb24_to_rgba_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    whole(width, || unsafe { rows::rgb24_to_rgba(src, dst, width) })
}

pub(super) fn gray8_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    whole(width, || unsafe { rows::gray8_to_rgb24(src, dst, width) })
}

pub(super) fn gray16le_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    whole(width, || unsafe {
        rows::gray16le_to_rgb24(src, dst, width)
    })
}

pub(super) fn rgb48le_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    swap: bool,
) -> Option<(SimdBackend, usize)> {
    whole(width, || unsafe {
        rows::rgb48le_to_rgb24(src, dst, width, swap)
    })
}

pub(super) fn yuyv_luma_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    whole(width, || unsafe { rows::yuyv_luma(src, dst, width) })
}

/// YUYV chroma of two rows.
pub(super) fn yuyv_chroma_row(
    a: &[u8],
    b: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    whole(width, || unsafe { rows::yuyv_chroma(a, b, dst, width) })
}

/// 2x2 box average.
pub(super) fn box2_row(
    top: &[u8],
    bottom: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    whole(width, || unsafe { rows::box2(top, bottom, dst, width) })
}

pub(super) fn reverse_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    bpp: usize,
) -> Option<(SimdBackend, core::ops::Range<usize>)> {
    whole(width, || unsafe { rows::reverse(src, dst, width, bpp) })
        .map(|(backend, done)| (backend, 0..done))
}

pub(super) fn transpose_tile(bpp: usize) -> Option<(SimdBackend, TransposeTile)> {
    if !available() {
        return None;
    }
    let tile: TransposeTile = match bpp {
        1 => transform::transpose_tile::<1>,
        2 => transform::transpose_tile::<2>,
        3 => transform::transpose_tile::<3>,
        4 => transform::transpose_tile::<4>,
        _ => return None,
    };
    Some((SimdBackend::Rvv, tile))
}

/// Run `leaf` on the row's whole pixel pairs when the CPU has vectors.
fn pairs(width: usize, leaf: impl FnOnce(usize)) -> Option<(SimdBackend, usize)> {
    let pairs = width / 2;
    if pairs == 0 || !available() {
        return None;
    }
    leaf(pairs);
    Some((SimdBackend::Rvv, pairs * 2))
}

pub(super) fn nv12_rgb_row(
    y: &[u8],
    uv: &[u8],
    dst: &mut [u8],
    width: usize,
    m: &YuvMatrix,
) -> Option<(SimdBackend, usize)> {
    pairs(width, |n| unsafe { rows::nv12_rgb(y, uv, dst, n, m) })
}

pub(super) fn i420_rgb_row(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    dst: &mut [u8],
    width: usize,
    m: &YuvMatrix,
) -> Option<(SimdBackend, usize)> {
    pairs(width, |n| unsafe { rows::i420_rgb(y, u, v, dst, n, m) })
}

pub(super) fn yuyv_rgb_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    m: &YuvMatrix,
) -> Option<(SimdBackend, usize)> {
    pairs(width, |n| unsafe { rows::yuyv_rgb(src, dst, n, m) })
}
