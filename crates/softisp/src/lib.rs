//! A software ISP: raw Bayer frames from a sensor to RGB24, NV12 / I420 or luma, in pure Rust.
//!
//! # Pipeline
//!
//! Each stage is optional and set by plain data ([`IspParams`], serde-able):
//!
//! 1. **Unpack** CSI-2 packed RAW10 / RAW12, 16-bit little-endian or 8-bit samples
//!    ([`RawPacking`]).
//! 2. **Black level** per CFA cell, **white balance and digital gain** per channel, and **lens
//!    shading** from a coarse gain grid, all folded into one multiply per sample. Samples are
//!    then 12-bit (0..=4095) whatever the input depth.
//! 3. **Demosaic**: bilinear, or Malvar-He-Cutler gradient-corrected ([`Demosaic`]). With
//!    [`Scale::Half`], each 2x2 quad becomes one pixel instead (no demosaic), the fast path to
//!    half size.
//! 4. **Colour correction matrix**.
//! 5. **Tone curve** (gamma, sRGB, or points) to 8 bits through a 4096-entry table (fp16:
//!    48 interpolated segments).
//! 6. **Output**: RGB24, NV12, I420 (BT.709 limited or BT.601 full range), or luma. Full-size
//!    luma comes straight from the mosaic (`(R + 2G + B) / 4` by a 3x3 binomial filter),
//!    without demosaic or colour correction.
//!
//! **Statistics** for AE / AWB ([`IspStats`]) are gathered in the same pass when
//! [`IspParams::stats`] asks: per-zone R/G/B sums and counts of unclipped quads, per-zone mean
//! luma and a luma histogram.
//!
//! The frame is processed row by row with a ring of a few front-end rows, so the working set
//! stays in L1/L2. [`SoftIsp::with_threads`] processes row bands in parallel on a persistent
//! pool of helper threads.
//!
//! # Arithmetic
//!
//! Two implementations of the per-pixel stages ([`Arithmetic`]): 12-bit fixed point, the
//! reference, on every CPU; and fp16 on CPUs with FP16 arithmetic (Cortex-A55/A76 and later),
//! where the front end, the bilinear demosaic with the colour matrix folded in, and the tone
//! curve (48 segments indexed by the fp16 exponent) run at about twice the speed, within a
//! code or two of the reference. [`Arithmetic::Auto`] (the default) picks fp16 there when the
//! parameters have a colour matrix or a tone curve.
//!
//! # Kernels
//!
//! Every per-pixel stage is a row kernel in [`simd`], following `styx_core::simd`: a scalar
//! oracle, x86 SSE2/SSSE3/AVX2 leaves chosen at run time, AArch64 NEON leaves, each tested for
//! exact equality with the oracle. The fp16 kernels ([`simd::half`]) have an exact software
//! fp16 oracle ([`simd::f16`](mod@simd::f16)).
//!
#![doc = include_str!("../PERFORMANCE.md")]
//!
//! # `no_std`
//!
//! Without the default `std` feature the crate is `no_std` + `alloc`: the kernels and
//! [`SoftIsp`] on the calling thread. `SoftIsp::with_threads` (the helper thread pool) needs
//! `std`; the SIMD leaves are then chosen from the target's compile-time features
//! (docs/portability.md).

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod format;
mod isp;
mod output;
pub mod params;
mod pipeline;
#[cfg(feature = "std")]
mod pool;
mod prepare;
mod prepare_half;
pub mod simd;
mod stats;

pub use format::{BAYER_FOURCCS, CfaPattern, Channel, RawFormat, RawPacking, bayer_fourcc};
pub use isp::{IspError, SoftIsp, Window, process};
pub use output::{OutputBuffers, Scale};
pub use params::{
    Arithmetic, BlackLevel, ColorMatrix, Demosaic, IspParams, LensShading, StatsConfig, ToneCurve,
    WhiteBalance, YuvMatrix,
};
pub use stats::{IspStats, ZoneStats};
