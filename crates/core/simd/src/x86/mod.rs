//! x86 leaves, chosen per call from the CPU's features (AVX2, then SSSE3, then SSE2). Each
//! returns the pixels it handled; the dispatcher finishes the row with the scalar kernel.

pub(super) mod rows;

use super::{SimdBackend, X86FeatureSet};

fn done(backend: SimdBackend, pixels: usize) -> Option<(SimdBackend, usize)> {
    (pixels > 0).then_some((backend, pixels))
}

pub(super) fn swap_rb24_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    let f = X86FeatureSet::detect();
    // SAFETY: the feature each needs was detected; the dispatcher sized
    // the rows for `width` pixels.
    if f.avx2 {
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
        return done(SimdBackend::X86Avx2, unsafe {
            rows::swap_rb24_avx2(src, dst, width)
        });
    }
    if f.ssse3 {
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
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
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
        return done(SimdBackend::X86Avx2, unsafe {
            rows::swap_rb32_avx2(src, dst, width)
        });
    }
    if f.ssse3 {
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
        return done(SimdBackend::X86Ssse3, unsafe {
            rows::swap_rb32_ssse3(src, dst, width)
        });
    }
    if f.sse2 {
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
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
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
        return done(SimdBackend::X86Avx2, unsafe {
            rows::x32_to_rgb24_avx2(src, dst, width, swap)
        });
    }
    if f.ssse3 {
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
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
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
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
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
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
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
        return done(SimdBackend::X86Avx2, unsafe {
            rows::gray8_to_rgb24_avx2(src, dst, width)
        });
    }
    if f.ssse3 {
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
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
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
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
                // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
                (SimdBackend::X86Ssse3, unsafe {
                    rows::rgb48le_to_rgb24_swap_ssse3(src, dst, width)
                })
            })
            .filter(|(_, n)| *n > 0);
    }
    if f.sse2 {
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
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
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
        return done(SimdBackend::X86Avx2, unsafe {
            rows::yuyv_luma_avx2(src, dst, width)
        });
    }
    if f.sse2 {
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
        return done(SimdBackend::X86Sse2, unsafe {
            rows::yuyv_luma_sse2(src, dst, width)
        });
    }
    None
}

pub(super) fn yuyv_chroma_row(
    a: &[u8],
    b: &[u8],
    dst: &mut [u8],
    width: usize,
) -> Option<(SimdBackend, usize)> {
    let f = X86FeatureSet::detect();
    if f.avx2 && width >= 32 {
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
        return done(SimdBackend::X86Avx2, unsafe {
            rows::yuyv_chroma_avx2(a, b, dst, width)
        });
    }
    if f.sse2 {
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
        return done(SimdBackend::X86Sse2, unsafe {
            rows::yuyv_chroma_sse2(a, b, dst, width)
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
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
        return done(SimdBackend::X86Avx2, unsafe {
            rows::box2_avx2(top, bottom, dst, width)
        });
    }
    if f.sse2 {
        // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
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
            // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
            0..unsafe { rows::reverse_u8_ssse3(src, dst, width) },
        ),
        3 if f.ssse3 => {
            // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
            let (start, end) = unsafe { rows::reverse_rgb_ssse3(src, dst, width) };
            (SimdBackend::X86Ssse3, start..end)
        }
        1 | 2 | 4 if f.sse2 => (
            SimdBackend::X86Sse2,
            // SAFETY: the feature this leaf needs was detected above, and the dispatcher sized the rows for `width` pixels.
            0..unsafe { rows::reverse_sse2(src, dst, width, bpp) },
        ),
        _ => return None,
    };
    (!range.is_empty()).then_some((backend, range))
}
