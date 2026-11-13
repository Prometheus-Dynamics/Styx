//! SIMD kernels for per-pixel work Styx does itself: channel swizzles, YUV to RGB, luma
//! extraction, box filters, reversal, and the JPEG and H.264 block kernels.
//!
//! Scalar kernels ([`scalar`]) are the correctness oracle. The x86 (SSE2, SSSE3, AVX2) and
//! AArch64 NEON modules are backend slots only: they must produce exactly the scalar result.
//! x86 leaves are chosen at run time from the detected CPU features (without `std`: at compile
//! time, from the target features); NEON is part of the AArch64 baseline. RISC-V vector
//! kernels run when the kernel reports the V extension (`AT_HWCAP`), and so do the 32-bit Arm
//! NEON kernels (ARMv7-A). Cargo features `x86`, `neon` and `rvv` (on by default) compile the
//! backends in; without them everything runs the scalar kernels.
//!
//! Every dispatcher returns the [`SimdBackend`] that did the vector part of the work.
//! Whether the vector leaves run at all is one process-wide [`SimdPolicy`] (`Auto` by default,
//! `Scalar` to run the oracle everywhere); the output is the same either way.
//!
//! The crate is `no_std` + `alloc`. The row kernels of the kernels above allocate nothing; the
//! soft ISP's tables ([`isp::ToneLut`], the fp16 tables) own their memory. `std` adds run-time
//! detection and the 32-bit Arm leaves; the tests use `std`.
//!
//! [`isp`] holds the software ISP's row kernels (the per-pixel stages of `styx-soft`): the same
//! layout as the kernels here, a scalar oracle with x86, NEON and RISC-V leaves under the same
//! [`SimdPolicy`].

#![no_std]
extern crate alloc;
#[cfg(any(feature = "std", test))]
#[macro_use]
extern crate std;
#[cfg(test)]
use std::{boxed::Box, vec::Vec};

use core::sync::atomic::{AtomicBool, Ordering};

pub mod h264;
pub mod isp;
pub mod jpeg;
pub mod scalar;

pub use scalar::{ColorLayout, YuvMatrix};

#[cfg(all(feature = "neon", target_arch = "aarch64"))]
mod neon;
#[cfg(all(
    feature = "neon",
    feature = "std",
    target_arch = "arm",
    target_os = "linux"
))]
mod neon32;
#[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
mod rvv;
#[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
mod x86;

#[cfg(test)]
mod h264_tests;
#[cfg(test)]
mod jpeg_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod yuv_tests;

/// Which implementation ran a kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SimdBackend {
    Scalar,
    X86Sse2,
    X86Ssse3,
    X86Avx2,
    /// AVX-512 with VBMI (byte permutes across a register): the leaves that gain from it.
    X86Avx512Vbmi,
    Neon,
    /// RISC-V vectors (RVV 1.0): feature `rvv`, on a CPU whose kernel reports the V extension.
    Rvv,
}

impl SimdBackend {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Scalar => "scalar",
            Self::X86Sse2 => "x86-sse2",
            Self::X86Ssse3 => "x86-ssse3",
            Self::X86Avx2 => "x86-avx2",
            Self::X86Avx512Vbmi => "x86-avx512vbmi",
            Self::Neon => "neon",
            Self::Rvv => "rvv",
        }
    }
}

/// x86 SIMD features Styx has kernels for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct X86FeatureSet {
    pub sse2: bool,
    pub ssse3: bool,
    pub avx2: bool,
    /// AVX-512 F, BW and VBMI together.
    pub avx512vbmi: bool,
}

impl X86FeatureSet {
    /// The running CPU's features (all false on other architectures, or without the `x86`
    /// feature). `is_x86_feature_detected!` caches its answer. Without `std` there is no run
    /// time detection: the features the target is compiled for (`-C target-feature`,
    /// `-C target-cpu`).
    pub fn detect() -> Self {
        #[cfg(all(
            feature = "std",
            feature = "x86",
            any(target_arch = "x86", target_arch = "x86_64")
        ))]
        {
            Self {
                sse2: std::is_x86_feature_detected!("sse2"),
                ssse3: std::is_x86_feature_detected!("ssse3"),
                avx2: std::is_x86_feature_detected!("avx2"),
                avx512vbmi: std::is_x86_feature_detected!("avx512f")
                    && std::is_x86_feature_detected!("avx512bw")
                    && std::is_x86_feature_detected!("avx512vbmi"),
            }
        }
        #[cfg(all(
            not(feature = "std"),
            feature = "x86",
            any(target_arch = "x86", target_arch = "x86_64")
        ))]
        {
            Self {
                sse2: cfg!(target_feature = "sse2"),
                ssse3: cfg!(target_feature = "ssse3"),
                avx2: cfg!(target_feature = "avx2"),
                avx512vbmi: cfg!(all(
                    target_feature = "avx512f",
                    target_feature = "avx512bw",
                    target_feature = "avx512vbmi"
                )),
            }
        }
        #[cfg(not(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64"))))]
        {
            Self::default()
        }
    }
}

/// Which kernels the dispatchers may run. `Auto` (the default) runs the strongest vector leaf
/// the CPU has; `Scalar` runs the oracle everywhere. It is a plain switch for a core where a
/// leaf measures slower than the scalar code, and for comparing the two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SimdPolicy {
    #[default]
    Auto,
    Scalar,
}

static SCALAR_ONLY: AtomicBool = AtomicBool::new(false);

/// Choose the kernels the dispatchers run, process-wide. Output is the same either way.
pub fn set_policy(policy: SimdPolicy) {
    SCALAR_ONLY.store(policy == SimdPolicy::Scalar, Ordering::Relaxed);
}

/// The current kernel policy.
pub fn policy() -> SimdPolicy {
    if vector_enabled() {
        SimdPolicy::Auto
    } else {
        SimdPolicy::Scalar
    }
}

/// Whether the vector leaves may run (the policy is `Auto`).
#[inline(always)]
pub(crate) fn vector_enabled() -> bool {
    !SCALAR_ONLY.load(Ordering::Relaxed)
}

/// The strongest backend compiled in and supported by this CPU.
pub fn strongest_backend() -> SimdBackend {
    if !vector_enabled() {
        return SimdBackend::Scalar;
    }
    if cfg!(all(feature = "neon", target_arch = "aarch64")) {
        return SimdBackend::Neon;
    }
    #[cfg(all(
        feature = "neon",
        feature = "std",
        target_arch = "arm",
        target_os = "linux"
    ))]
    if neon32::available() {
        return SimdBackend::Neon;
    }
    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
    if rvv::available() {
        return SimdBackend::Rvv;
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

/// Whether 32-bit Arm NEON leaves run on this CPU (always false on other targets).
pub fn has_neon32() -> bool {
    #[cfg(all(
        feature = "neon",
        feature = "std",
        target_arch = "arm",
        target_os = "linux"
    ))]
    if neon32::available() {
        return true;
    }
    false
}

/// Whether RISC-V vector leaves run on this CPU (always false on other targets).
pub fn has_rvv() -> bool {
    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
    if rvv::available() {
        return true;
    }
    false
}

/// The first compiled-in backend leaf's result: `Some((backend, pixels done))` when it handled a
/// prefix of the row, `None` when no leaf applies (unsupported CPU, row too short).
macro_rules! leaf {
    ($name:ident($($arg:expr),* $(,)?)) => {{
        #[allow(unused_mut, unused_assignments)]
        let mut outcome: Option<(SimdBackend, usize)> = None;
        if vector_enabled() {
        #[cfg(all(feature = "neon", target_arch = "aarch64"))]
        {
            outcome = neon::$name($($arg),*);
        }
        #[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            outcome = x86::$name($($arg),*);
        }
        #[cfg(all(
            feature = "neon",
            feature = "std",
            target_arch = "arm",
            target_os = "linux"
        ))]
        {
            outcome = neon32::$name($($arg),*);
        }
        #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
        {
            outcome = rvv::$name($($arg),*);
        }
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

/// YUYV chroma of two rows averaged (NV12's chroma row: `width` bytes of UV pairs).
pub fn yuyv_chroma_row(a: &[u8], b: &[u8], dst: &mut [u8], width: usize) -> SimdBackend {
    let (a, b, dst) = (&a[..width * 2], &b[..width * 2], &mut dst[..width]);
    let outcome = leaf!(yuyv_chroma_row(a, b, dst, width));
    finish(outcome, width, |d, n| {
        scalar::yuyv_chroma_row(&a[d * 2..], &b[d * 2..], &mut dst[d..], n)
    })
}

/// The 32-bit Arm and RISC-V vector leaf's result for the YUV → RGB rows (pixel pairs done).
/// These cover the targets the x86 and AArch64 leaves do not: 32-bit Arm and RISC-V vectors.
macro_rules! yuv_leaf {
    ($name:ident($($arg:expr),* $(,)?)) => {{
        #[allow(unused_mut, unused_assignments)]
        let mut outcome: Option<(SimdBackend, usize)> = None;
        if vector_enabled() {
        #[cfg(all(
            feature = "neon",
            feature = "std",
            target_arch = "arm",
            target_os = "linux"
        ))]
        {
            outcome = neon32::$name($($arg),*);
        }
        #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
        {
            outcome = rvv::$name($($arg),*);
        }
        }
        outcome
    }};
}

/// NV12 → RGB24 ([`YuvMatrix`]): `width` luma bytes and `width.div_ceil(2)` UV pairs.
pub fn nv12_rgb_row(
    y: &[u8],
    uv: &[u8],
    dst: &mut [u8],
    width: usize,
    m: &YuvMatrix,
) -> SimdBackend {
    let (y, uv, dst) = (
        &y[..width],
        &uv[..width.div_ceil(2) * 2],
        &mut dst[..width * 3],
    );
    let outcome = yuv_leaf!(nv12_rgb_row(y, uv, dst, width, m));
    finish(outcome, width, |d, n| {
        scalar::nv12_rgb_row(&y[d..], &uv[d..], &mut dst[d * 3..], n, m)
    })
}

/// I420 → RGB24 ([`YuvMatrix`]): `width` luma bytes and `width.div_ceil(2)` of each chroma
/// (YV12: `u` and `v` swapped).
pub fn i420_rgb_row(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    dst: &mut [u8],
    width: usize,
    m: &YuvMatrix,
) -> SimdBackend {
    let half = width.div_ceil(2);
    let (y, u, v, dst) = (&y[..width], &u[..half], &v[..half], &mut dst[..width * 3]);
    let outcome = yuv_leaf!(i420_rgb_row(y, u, v, dst, width, m));
    finish(outcome, width, |d, n| {
        scalar::i420_rgb_row(&y[d..], &u[d / 2..], &v[d / 2..], &mut dst[d * 3..], n, m)
    })
}

/// YUYV → RGB24 ([`YuvMatrix`]): `width.div_ceil(2)` 4-byte groups.
pub fn yuyv_rgb_row(src: &[u8], dst: &mut [u8], width: usize, m: &YuvMatrix) -> SimdBackend {
    let (src, dst) = (&src[..width.div_ceil(2) * 4], &mut dst[..width * 3]);
    let outcome = yuv_leaf!(yuyv_rgb_row(src, dst, width, m));
    finish(outcome, width, |d, n| {
        scalar::yuyv_rgb_row(&src[d * 2..], &mut dst[d * 3..], n, m)
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
    let mut outcome: Option<(SimdBackend, core::ops::Range<usize>)> = None;
    if vector_enabled() {
        #[cfg(all(feature = "neon", target_arch = "aarch64"))]
        {
            outcome = neon::reverse_row(src, dst, width, bpp);
        }
        #[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            outcome = x86::reverse_row(src, dst, width, bpp);
        }
        #[cfg(all(
            feature = "neon",
            feature = "std",
            target_arch = "arm",
            target_os = "linux"
        ))]
        {
            outcome = neon32::reverse_row(src, dst, width, bpp);
        }
        #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
        {
            outcome = rvv::reverse_row(src, dst, width, bpp);
        }
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
