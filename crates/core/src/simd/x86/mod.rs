//! x86 leaves, chosen per call from the CPU's features (AVX2, then SSSE3, then SSE2). Each
//! returns the pixels it handled; the dispatcher finishes the row with the scalar kernel.

pub(super) mod rows;
pub(super) mod transform;

use super::{SimdBackend, TransposeTile, X86FeatureSet};

fn done(backend: SimdBackend, pixels: usize) -> Option<(SimdBackend, usize)> {
    (pixels > 0).then_some((backend, pixels))
}

pub(super) fn swap_rb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    let f = X86FeatureSet::detect();
    // SAFETY (all leaves below): the feature each needs was detected; the dispatcher sized
    // the rows for `width` pixels.
    if f.avx2 {
        return done(SimdBackend::X86Avx2, unsafe {
            rows::swap_rb24_avx2(src, dst, width)
        });
    }
    if f.ssse3 {
        return done(SimdBackend::X86Ssse3, unsafe {
            rows::swap_rb24_ssse3(src, dst, width)
        });
    }
    None
}

pub(super) fn swap_rb32_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    let f = X86FeatureSet::detect();
    if f.avx2 && width >= 8 {
        return done(SimdBackend::X86Avx2, unsafe {
            rows::swap_rb32_avx2(src, dst, width)
        });
    }
    if f.ssse3 {
        return done(SimdBackend::X86Ssse3, unsafe {
            rows::swap_rb32_ssse3(src, dst, width)
        });
    }
    if f.sse2 {
        return done(SimdBackend::X86Sse2, unsafe {
            rows::swap_rb32_sse2(src, dst, width)
        });
    }
    None
}

pub(super) fn x32_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    swap: bool,
) -> Option<(SimdBackend, usize)> {
    let f = X86FeatureSet::detect();
    if f.avx2 {
        return done(SimdBackend::X86Avx2, unsafe {
            rows::x32_to_rgb24_avx2(src, dst, width, swap)
        });
    }
    if f.ssse3 {
        return done(SimdBackend::X86Ssse3, unsafe {
            rows::x32_to_rgb24_ssse3(src, dst, width, swap)
        });
    }
    None
}

pub(super) fn rgb_to_luma_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    layout: super::ColorLayout,
) -> Option<(SimdBackend, usize)> {
    if X86FeatureSet::detect().ssse3 {
        return done(SimdBackend::X86Ssse3, unsafe {
            rows::rgb_to_luma_ssse3(src, dst, width, layout)
        });
    }
    None
}

pub(super) fn rgb24_to_rgba_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    if X86FeatureSet::detect().ssse3 {
        return done(SimdBackend::X86Ssse3, unsafe {
            rows::rgb24_to_rgba_ssse3(src, dst, width)
        });
    }
    None
}

pub(super) fn gray8_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    let f = X86FeatureSet::detect();
    if f.avx2 {
        return done(SimdBackend::X86Avx2, unsafe {
            rows::gray8_to_rgb24_avx2(src, dst, width)
        });
    }
    if f.ssse3 {
        return done(SimdBackend::X86Ssse3, unsafe {
            rows::gray8_to_rgb24_ssse3(src, dst, width)
        });
    }
    None
}

pub(super) fn gray16le_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    if X86FeatureSet::detect().ssse3 {
        return done(SimdBackend::X86Ssse3, unsafe {
            rows::gray16le_to_rgb24_ssse3(src, dst, width)
        });
    }
    None
}

pub(super) fn rgb48le_to_rgb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    swap: bool,
) -> Option<(SimdBackend, usize)> {
    let f = X86FeatureSet::detect();
    if swap {
        return f
            .ssse3
            .then(|| {
                (SimdBackend::X86Ssse3, unsafe {
                    rows::rgb48le_to_rgb24_swap_ssse3(src, dst, width)
                })
            })
            .filter(|(_, n)| *n > 0);
    }
    if f.sse2 {
        return done(SimdBackend::X86Sse2, unsafe {
            rows::rgb48le_to_rgb24_sse2(src, dst, width)
        });
    }
    None
}

pub(super) fn yuyv_luma_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    let f = X86FeatureSet::detect();
    if f.avx2 && width >= 32 {
        return done(SimdBackend::X86Avx2, unsafe {
            rows::yuyv_luma_avx2(src, dst, width)
        });
    }
    if f.sse2 {
        return done(SimdBackend::X86Sse2, unsafe {
            rows::yuyv_luma_sse2(src, dst, width)
        });
    }
    None
}

pub(super) fn box2_row(
    top: &[u8],
    bottom: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    let f = X86FeatureSet::detect();
    if f.avx2 && width >= 32 {
        return done(SimdBackend::X86Avx2, unsafe {
            rows::box2_avx2(top, bottom, dst, width)
        });
    }
    if f.sse2 {
        return done(SimdBackend::X86Sse2, unsafe {
            rows::box2_sse2(top, bottom, dst, width)
        });
    }
    None
}

pub(super) fn reverse_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    bpp: usize,
) -> Option<(SimdBackend, core::ops::Range<usize>)> {
    let f = X86FeatureSet::detect();
    let (backend, range) = match bpp {
        1 if f.ssse3 => (
            SimdBackend::X86Ssse3,
            0..unsafe { rows::reverse_u8_ssse3(src, dst, width) },
        ),
        3 if f.ssse3 => {
            let (start, end) = unsafe { rows::reverse_rgb_ssse3(src, dst, width) };
            (SimdBackend::X86Ssse3, start..end)
        }
        1 | 2 | 4 if f.sse2 => (
            SimdBackend::X86Sse2,
            0..unsafe { rows::reverse_sse2(src, dst, width, bpp) },
        ),
        _ => return None,
    };
    (!range.is_empty()).then_some((backend, range))
}

pub(super) fn transpose_tile(bpp: usize) -> Option<(SimdBackend, TransposeTile)> {
    let f = X86FeatureSet::detect();
    if bpp == 3 {
        let tile: TransposeTile = transform::transpose_tile_rgb_ssse3;
        return f.ssse3.then_some((SimdBackend::X86Ssse3, tile));
    }
    if !f.sse2 {
        return None;
    }
    let tile: TransposeTile = match bpp {
        1 => transform::transpose_tile_u8_sse2,
        2 => transform::transpose_tile_u16_sse2,
        4 => transform::transpose_tile_u32_sse2,
        _ => return None,
    };
    Some((SimdBackend::X86Sse2, tile))
}
