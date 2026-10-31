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

/// The ISP32 generation a buffer or a device is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// The full ISP32: RV1106 (the driver's `ISP_V32`, `rv1106_isp_match_data`).
    Isp32,
    /// The ISP32-lite: RK3562 (the driver's `ISP_V32_L`, `rk3562_isp_match_data`).
    Isp32Lite,
}

/// The video nodes an ISP32 exposes, as the vendor driver registers them
/// (`rkisp_register_stream_v32` and the `*_VDEV_NAME` macros of `capture.h`).
pub mod nodes {
    /// The main path (both variants).
    pub const MAINPATH: &str = "rkisp_mainpath";
    /// The self path, the second, scaled output (both variants).
    pub const SELFPATH: &str = "rkisp_selfpath";
    /// The bypass path (ISP32 only; the path diagram is at `capture_v32.c:17-25`).
    pub const BYPASSPATH: &str = "rkisp_bypasspath";
    /// The main path's 4x4-sampled output (ISP32 only, `MPDS`).
    pub const MAINPATH_4X4: &str = "rkisp_mainpath_4x4sampling";
    /// The bypass path's 4x4-sampled output (ISP32 only, `BPDS`).
    pub const BYPASSPATH_4X4: &str = "rkisp_bypasspath_4x4sampling";
    /// The luma path (ISP32 only).
    pub const LUMAPATH: &str = "rkisp_lumapath";
    /// The IQ tool's virtual output (both variants).
    pub const IQTOOL: &str = "rkisp_iqtool";
}

impl Variant {
    /// The video nodes this variant exposes. The pyramid levels come from [`nodes::SELFPATH`]
    /// (the scaled output) and, on the full ISP32 only, the 4x4-sampled outputs
    /// ([`nodes::MAINPATH_4X4`], [`nodes::BYPASSPATH_4X4`]); the BSP has no 8x8 path.
    pub fn output_nodes(self) -> &'static [&'static str] {
        match self {
            Self::Isp32 => &[
                nodes::MAINPATH,
                nodes::SELFPATH,
                nodes::BYPASSPATH,
                nodes::MAINPATH_4X4,
                nodes::BYPASSPATH_4X4,
                nodes::LUMAPATH,
                nodes::IQTOOL,
            ],
            Self::Isp32Lite => &[nodes::MAINPATH, nodes::SELFPATH, nodes::IQTOOL],
        }
    }
}

#[cfg(test)]
mod tests;
