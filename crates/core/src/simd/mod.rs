//! SIMD kernels for per-pixel work Styx does itself: channel swizzles, luma extraction, box
//! filters, and rotation/mirroring.
//!
//! Scalar kernels ([`scalar`]) are the correctness oracle. The x86 (SSE2, SSSE3, AVX2) and
//! AArch64 NEON modules are backend slots only: they must produce exactly the scalar result.
//! x86 leaves are chosen at run time from the detected CPU features; NEON is part of the
//! AArch64 baseline. Cargo features `x86` and `neon` (on by default) compile the backends in;
//! without them everything runs the scalar kernels.
//!
//! Every dispatcher returns the [`SimdBackend`] that did the vector part of the work.

pub mod scalar;

pub use scalar::ColorLayout;

#[cfg(all(feature = "neon", target_arch = "aarch64"))]
mod neon;
#[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
mod x86;

#[cfg(test)]
mod tests;

/// Which implementation ran a kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SimdBackend {
    Scalar,
    X86Sse2,
    X86Ssse3,
    X86Avx2,
    Neon,
}

impl SimdBackend {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Scalar => "scalar",
            Self::X86Sse2 => "x86-sse2",
            Self::X86Ssse3 => "x86-ssse3",
            Self::X86Avx2 => "x86-avx2",
            Self::Neon => "neon",
        }
    }
}

/// x86 SIMD features Styx has kernels for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct X86FeatureSet {
    pub sse2: bool,
    pub ssse3: bool,
    pub avx2: bool,
}

impl X86FeatureSet {
    /// The running CPU's features (all false on other architectures, or without the `x86`
    /// feature). `is_x86_feature_detected!` caches its answer.
    pub fn detect() -> Self {
        #[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            Self {
                sse2: std::is_x86_feature_detected!("sse2"),
                ssse3: std::is_x86_feature_detected!("ssse3"),
                avx2: std::is_x86_feature_detected!("avx2"),
            }
        }
        #[cfg(not(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64"))))]
        {
            Self::default()
        }
    }
}

/// The strongest backend compiled in and supported by this CPU.
pub fn strongest_backend() -> SimdBackend {
    if cfg!(all(feature = "neon", target_arch = "aarch64")) {
        return SimdBackend::Neon;
    }
    let x86 = X86FeatureSet::detect();
    if x86.avx2 {
        SimdBackend::X86Avx2
    } else if x86.ssse3 {
        SimdBackend::X86Ssse3
    } else if x86.sse2 {
        SimdBackend::X86Sse2
    } else {
        SimdBackend::Scalar
    }
}

/// The first compiled-in backend leaf's result: `Some((backend, pixels done))` when it handled a
/// prefix of the row, `None` when no leaf applies (unsupported CPU, row too short).
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
            outcome = x86::$name($($arg),*);
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

/// BGR ↔ RGB for `width` 3-byte pixels.
pub fn swap_rb24_row(src: &[u8], dst: &mut [u8], width: usize) -> SimdBackend {
    let (src, dst) = (&src[..width * 3], &mut dst[..width * 3]);
    let outcome = leaf!(swap_rb24_row(src, dst, width));
    finish(outcome, width, |d, n| {
        scalar::swap_rb24_row(&src[d * 3..], &mut dst[d * 3..], n)
    })
}

/// BGRA ↔ RGBA for `width` 4-byte pixels.
pub fn swap_rb32_row(src: &[u8], dst: &mut [u8], width: usize) -> SimdBackend {
    let (src, dst) = (&src[..width * 4], &mut dst[..width * 4]);
    let outcome = leaf!(swap_rb32_row(src, dst, width));
    finish(outcome, width, |d, n| {
        scalar::swap_rb32_row(&src[d * 4..], &mut dst[d * 4..], n)
    })
}

/// 4-byte pixels (RGBA/RGBX, or BGRA/BGRX with `swap`) to RGB24.
pub fn x32_to_rgb24_row(src: &[u8], dst: &mut [u8], width: usize, swap: bool) -> SimdBackend {
    let (src, dst) = (&src[..width * 4], &mut dst[..width * 3]);
    let outcome = leaf!(x32_to_rgb24_row(src, dst, width, swap));
    finish(outcome, width, |d, n| {
        scalar::x32_to_rgb24_row(&src[d * 4..], &mut dst[d * 3..], n, swap)
    })
}

/// Luma from colour pixels: `(77 R + 150 G + 29 B) >> 8`.
pub fn rgb_to_luma_row(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    layout: ColorLayout,
) -> SimdBackend {
    let bpp = layout.bytes_per_pixel();
    let (src, dst) = (&src[..width * bpp], &mut dst[..width]);
    let outcome = leaf!(rgb_to_luma_row(src, dst, width, layout));
    finish(outcome, width, |d, n| {
        scalar::rgb_to_luma_row(&src[d * bpp..], &mut dst[d..], n, layout)
    })
}

/// RGB24 to RGBA with alpha 255.
pub fn rgb24_to_rgba_row(src: &[u8], dst: &mut [u8], width: usize) -> SimdBackend {
    let (src, dst) = (&src[..width * 3], &mut dst[..width * 4]);
    let outcome = leaf!(rgb24_to_rgba_row(src, dst, width));
    finish(outcome, width, |d, n| {
        scalar::rgb24_to_rgba_row(&src[d * 3..], &mut dst[d * 4..], n)
    })
}

/// Grey to RGB24.
pub fn gray8_to_rgb24_row(src: &[u8], dst: &mut [u8], width: usize) -> SimdBackend {
    let (src, dst) = (&src[..width], &mut dst[..width * 3]);
    let outcome = leaf!(gray8_to_rgb24_row(src, dst, width));
    finish(outcome, width, |d, n| {
        scalar::gray8_to_rgb24_row(&src[d..], &mut dst[d * 3..], n)
    })
}

/// Little-endian 16-bit grey to RGB24 (high byte).
pub fn gray16le_to_rgb24_row(src: &[u8], dst: &mut [u8], width: usize) -> SimdBackend {
    let (src, dst) = (&src[..width * 2], &mut dst[..width * 3]);
    let outcome = leaf!(gray16le_to_rgb24_row(src, dst, width));
    finish(outcome, width, |d, n| {
        scalar::gray16le_to_rgb24_row(&src[d * 2..], &mut dst[d * 3..], n)
    })
}

/// Little-endian 16-bit RGB (BGR with `swap`) to RGB24 (high bytes).
pub fn rgb48le_to_rgb24_row(src: &[u8], dst: &mut [u8], width: usize, swap: bool) -> SimdBackend {
    let (src, dst) = (&src[..width * 6], &mut dst[..width * 3]);
    let outcome = leaf!(rgb48le_to_rgb24_row(src, dst, width, swap));
    finish(outcome, width, |d, n| {
        scalar::rgb48le_to_rgb24_row(&src[d * 6..], &mut dst[d * 3..], n, swap)
    })
}

/// YUYV to luma.
pub fn yuyv_luma_row(src: &[u8], dst: &mut [u8], width: usize) -> SimdBackend {
    let (src, dst) = (&src[..width * 2], &mut dst[..width]);
    let outcome = leaf!(yuyv_luma_row(src, dst, width));
    finish(outcome, width, |d, n| {
        scalar::yuyv_luma_row(&src[d * 2..], &mut dst[d..], n)
    })
}

/// 2x2 box average of two rows into `width` outputs.
pub fn box2_row(top: &[u8], bottom: &[u8], dst: &mut [u8], width: usize) -> SimdBackend {
    let (top, bottom, dst) = (&top[..width * 2], &bottom[..width * 2], &mut dst[..width]);
    let outcome = leaf!(box2_row(top, bottom, dst, width));
    finish(outcome, width, |d, n| {
        scalar::box2_row(&top[d * 2..], &bottom[d * 2..], &mut dst[d..], n)
    })
}

/// Pixel order reversed (`dst[i] = src[width - 1 - i]`) for 1- to 4-byte pixels.
pub fn reverse_row(src: &[u8], dst: &mut [u8], width: usize, bpp: usize) -> SimdBackend {
    let (src, dst) = (&src[..width * bpp], &mut dst[..width * bpp]);
    #[allow(unused_mut, unused_assignments)]
    let mut outcome: Option<(SimdBackend, std::ops::Range<usize>)> = None;
    #[cfg(all(feature = "neon", target_arch = "aarch64"))]
    {
        outcome = neon::reverse_row(src, dst, width, bpp);
    }
    #[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
    {
        outcome = x86::reverse_row(src, dst, width, bpp);
    }
    // The leaves reverse the source pixels in `done`; finish those before and after it.
    let (backend, done) = outcome.unwrap_or((SimdBackend::Scalar, 0..0));
    for range in [0..done.start, done.end..width] {
        for i in range {
            let d = (width - 1 - i) * bpp;
            dst[d..d + bpp].copy_from_slice(&src[i * bpp..(i + 1) * bpp]);
        }
    }
    backend
}

/// How a packed frame is reoriented: `out(x, y)` reads `in(y, x)` when `transpose`, then the
/// output is mirrored horizontally (`flip_x`) and/or vertically (`flip_y`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Orientation {
    pub transpose: bool,
    pub flip_x: bool,
    pub flip_y: bool,
}

impl Orientation {
    /// Clockwise rotation by `quarter_turns` x 90°, then an optional horizontal mirror.
    pub const fn rotation(quarter_turns: u8, mirror: bool) -> Self {
        let (transpose, flip_x, flip_y) = match quarter_turns % 4 {
            0 => (false, false, false),
            1 => (true, true, false),
            2 => (false, true, true),
            _ => (true, false, true),
        };
        Self {
            transpose,
            flip_x: flip_x != mirror,
            flip_y,
        }
    }
}

/// An 8x8 tile transpose leaf (see [`scalar::transpose_tile`]).
pub type TransposeTile = unsafe fn(*const u8, usize, *mut u8, isize, bool);

fn transpose_tile_leaf(bpp: usize) -> (SimdBackend, TransposeTile) {
    #[cfg(all(feature = "neon", target_arch = "aarch64"))]
    if let Some(leaf) = neon::transpose_tile(bpp) {
        return leaf;
    }
    #[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
    if let Some(leaf) = x86::transpose_tile(bpp) {
        return leaf;
    }
    let tile: TransposeTile = match bpp {
        1 => scalar::transpose_tile::<1>,
        2 => scalar::transpose_tile::<2>,
        3 => scalar::transpose_tile::<3>,
        _ => scalar::transpose_tile::<4>,
    };
    (SimdBackend::Scalar, tile)
}

/// Reorient a packed frame of `size` = (width, height) `bpp`-byte pixels (1 to 4) into `dst`,
/// whose rows are `dst_stride` bytes apart and whose size is the reoriented one.
///
/// # Panics
/// When `bpp` is not 1 to 4 or a buffer is too small.
pub fn transform_packed(
    src: &[u8],
    src_stride: usize,
    dst: &mut [u8],
    dst_stride: usize,
    size: (usize, usize),
    bpp: usize,
    orientation: Orientation,
) -> SimdBackend {
    let (width, height) = size;
    assert!((1..=4).contains(&bpp), "1 to 4 bytes per pixel");
    let (w_out, h_out) = if orientation.transpose {
        (height, width)
    } else {
        (width, height)
    };
    if width == 0 || height == 0 {
        return SimdBackend::Scalar;
    }
    assert!(
        src.len() >= (height - 1) * src_stride + width * bpp,
        "source too small"
    );
    assert!(
        dst.len() >= (h_out - 1) * dst_stride + w_out * bpp,
        "destination too small"
    );
    let out_row = |y: usize| if orientation.flip_y { h_out - 1 - y } else { y };
    if !orientation.transpose {
        let mut backend = SimdBackend::Scalar;
        for y in 0..h_out {
            let src_row = &src[out_row(y) * src_stride..][..width * bpp];
            let dst_row = &mut dst[y * dst_stride..][..width * bpp];
            if orientation.flip_x {
                backend = reverse_row(src_row, dst_row, width, bpp);
            } else {
                dst_row.copy_from_slice(src_row);
            }
        }
        return backend;
    }
    // Transposed: input tile (rows r.., columns c..) becomes output rows c.., columns r...
    let (backend, tile) = transpose_tile_leaf(bpp);
    let out_x = |x: usize| if orientation.flip_x { w_out - 1 - x } else { x };
    let (full_rows, full_cols) = (height / 8 * 8, width / 8 * 8);
    for r in (0..full_rows).step_by(8) {
        for c in (0..full_cols).step_by(8) {
            // Output row c, leftmost column of the tile after mirroring.
            let first_row = out_row(c);
            let first_col = if orientation.flip_x { w_out - r - 8 } else { r };
            let stride = if orientation.flip_y {
                -(dst_stride as isize)
            } else {
                dst_stride as isize
            };
            // SAFETY: the tile's 8x8 source pixels and 8 output rows of 8 pixels are inside
            // the buffers checked above (full tiles only; edges are copied below).
            unsafe {
                tile(
                    src.as_ptr().add(r * src_stride + c * bpp),
                    src_stride,
                    dst.as_mut_ptr()
                        .add(first_row * dst_stride + first_col * bpp),
                    stride,
                    orientation.flip_x,
                );
            }
        }
    }
    // Edges: input rows below the last full tile row, and columns right of the last full
    // tile column.
    let mut copy = |in_x: usize, in_y: usize| {
        let (x, y) = (out_x(in_y), out_row(in_x));
        let s = in_y * src_stride + in_x * bpp;
        dst[y * dst_stride + x * bpp..][..bpp].copy_from_slice(&src[s..s + bpp]);
    };
    for in_y in 0..height {
        let first = if in_y < full_rows { full_cols } else { 0 };
        for in_x in first..width {
            copy(in_x, in_y);
        }
    }
    backend
}
