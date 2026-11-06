//! Rockchip ISP21 (RK3566 / RK3568) and ISP30 (RK3588 / RK3588S) image signal processors,
//! driven from Rust through the vendor kernel's `rkisp` driver: the parameter buffer (one per
//! frame, written by Styx's 3A instead of Rockchip's `rkaiq`) and the statistics buffer (read
//! into Styx's 3A input).
//!
//! - [`uapi`]: the buffers' layouts, Lemnos's (`lemnos-rkisp-sys`, from the kernel's
//!   `rk-isp21-config.h`, `rk-isp3-config.h` and `rk-isp32-config.h`).
//! - [`Params`]: a parameter buffer for a frame: the statistics Styx's 3A reads, black level,
//!   white balance gains, lens shading, colour matrix, output gamma.
//! - [`statistics`]: a statistics buffer as [`styx_algo::Statistics`].
//! - `device` (feature `device`): the vendor driver's ISP, parameter, statistics and main
//!   path nodes, re-exported from `lemnos-drivers-rkisp`.
//!
//! [`Isp`] (Lemnos's) names a system's ISP from the SoC; [`IspBuffers`] builds its parameter
//! buffer ([`AnyParams`]) and reads its statistics whatever the generation.
//!
//! [`Generation`] selects the layout. The two share the modules and most of the layouts; the
//! lens shading (8x8 sectors on ISP21, 16x16 on ISP30) and the output gamma (45 samples on
//! ISP21, 49 on ISP30) differ.
//!
//! [`isp32`] is the same for the ISP32 of the RV1106 and the ISP32-lite of the RK3562 (its
//! buffers differ; its builder and statistics take a [`isp32::Variant`]), written, untested on
//! board as well.
//!
//! Status: built from the uAPI headers and checked against the vendor driver's source; the
//! ISP21 path is written and unit-tested against fabricated buffers, not yet run on an
//! RK3566 (written, untested on board). The ISP30 path has not run on an RK3588 either. The
//! fixed-point formats the headers do not state (gain and matrix scales, the statistics' bit
//! depths, the ISP21 lens shading's vertex layout) are named constants, to be confirmed on
//! the device.

#![no_std]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "device")]
pub use lemnos_drivers_rkisp::device;
mod isp;
pub mod isp32;

mod params;
mod stats;

pub use isp::{AnyParams, IspBuffers};
pub use lemnos_drivers_rkisp::{Isp, ParamBuffer, uapi};
pub use params::{
    CCM_ONE, GAIN_ONE, GAMMA_OUT_MAX, Generation, ISP21_GAMMA_SAMPLES, ISP21_LSC_SECTORS,
    ISP30_LSC_SECTORS, LSC_ONE, Params,
};
pub use stats::{AE_G_FULL, AE_RB_FULL, AE_ZONES, statistics};

#[cfg(test)]
mod tests;
