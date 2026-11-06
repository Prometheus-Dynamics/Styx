//! Rockchip ISP32 (RV1106) and ISP32-lite (RK3562): the same parameter and statistics buffers
//! as ISP30 ([`crate::Params`]) in the layouts of `rk-isp32-config.h`, with the differences the
//! vendor driver makes between the two.
//!
//! - [`Variant`]: which generation (the driver's `ISP_V32` for RV1106, `ISP_V32_L` for RK3562).
//! - [`Params`]: a parameter buffer for a frame, as ISP30's builder does, but with the big
//!   histogram the variant has (`rawhist1` on ISP32, `rawhist3` on ISP32-lite).
//! - [`statistics`]: a statistics buffer as [`styx_algo::Statistics`], decoding each variant's
//!   AE zones (packed 4-byte zones on ISP32, 6-byte zones on ISP32-lite).
//! - [`Variant::output_nodes`]: the video nodes each variant's ISP exposes, the selfpath among
//!   them.
//!
//! Status: written, untested on board. The buffers' layouts are checked against the C
//! compiler's sizes and offsets, and the builder and the statistics are unit-tested against
//! fabricated buffers. Nothing here has run on an RV1106 or RK3562 yet. The fixed-point scales
//! are the ISP30 ones (see [`Params`]), to be confirmed on the device.

pub mod params;
pub mod stats;

pub use params::Params;
pub use stats::statistics;

pub use lemnos_drivers_rkisp::isp32::{Variant, nodes};

#[cfg(test)]
mod tests;
