//! Rockchip ISP30, the RK3588 / RK3588S image signal processor, driven from Rust through the
//! vendor kernel's `rkisp` driver: its parameter buffer (one per frame, written by Styx's 3A
//! instead of Rockchip's `rkaiq`) and its statistics buffer (read into Styx's 3A input).
//!
//! - [`uapi`]: the buffers' layouts, from the kernel's `rk-isp3-config.h` (MIT), checked
//!   against the C compiler's.
//! - [`Params`]: a parameter buffer for a frame: the statistics Styx's 3A reads, black level,
//!   white balance gains, lens shading, colour matrix, output gamma.
//! - [`statistics`]: a statistics buffer as [`styx_algo::Statistics`].
//! - `device` (feature `device`): the vendor driver's ISP, parameter, statistics and main
//!   path nodes.
//!
//! Status: built from the uAPI headers only; nothing here has run on an RK3588 yet. The
//! fixed-point formats the headers do not state (gain and matrix scales, the statistics' bit
//! depths) are named constants, taken from how Rockchip's own 3A fills the same fields, to be
//! confirmed on the device.

#![no_std]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "device")]
pub mod device;

mod params;
mod stats;
pub mod uapi;

pub use params::{CCM_ONE, GAIN_ONE, GAMMA_OUT_MAX, LSC_ONE, Params};
pub use stats::{AE_G_FULL, AE_RB_FULL, AE_ZONES, statistics};

#[cfg(test)]
mod tests;
