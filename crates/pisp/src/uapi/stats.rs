//! `pisp_statistics.h`: the front end statistics buffer (`rp1-cfe-fe_stats`,
//! `V4L2_META_FMT_RPI_FE_STATS`), one per frame.

use bytemuck::{Pod, Zeroable};

/// Floating statistics regions.
pub const FLOATING_STATS_NUM_ZONES: usize = 4;
/// AGC luminance histogram bins.
pub const AGC_STATS_NUM_BINS: usize = 1024;
/// AGC grid side.
pub const AGC_STATS_SIZE: usize = 16;
/// AGC zones.
pub const AGC_STATS_NUM_ZONES: usize = AGC_STATS_SIZE * AGC_STATS_SIZE;
/// AGC row sums.
pub const AGC_STATS_NUM_ROW_SUMS: usize = 512;
/// AWB grid side.
pub const AWB_STATS_SIZE: usize = 32;
/// AWB zones.
pub const AWB_STATS_NUM_ZONES: usize = AWB_STATS_SIZE * AWB_STATS_SIZE;
/// CDAF grid side.
pub const CDAF_STATS_SIZE: usize = 8;
/// CDAF figures of merit.
pub const CDAF_STATS_NUM_FOMS: usize = CDAF_STATS_SIZE * CDAF_STATS_SIZE;

/// `pisp_agc_statistics_zone`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct RawAgcZone {
    /// Sum of Y.
    pub y_sum: u64,
    /// Pixels counted.
    pub counted: u32,
    /// Padding.
    pub pad: u32,
}

/// `pisp_agc_statistics`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct RawAgcStatistics {
    /// Row sums.
    pub row_sums: [u32; AGC_STATS_NUM_ROW_SUMS],
    /// Weighted Y histogram.
    pub histogram: [u32; AGC_STATS_NUM_BINS],
    /// Floating regions.
    pub floating: [RawAgcZone; FLOATING_STATS_NUM_ZONES],
}

/// `pisp_awb_statistics_zone`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct RawAwbZone {
    /// Sum of R.
    pub r_sum: u32,
    /// Sum of G.
    pub g_sum: u32,
    /// Sum of B.
    pub b_sum: u32,
    /// Pixels counted.
    pub counted: u32,
}

/// `pisp_awb_statistics`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct RawAwbStatistics {
    /// Grid zones, row-major.
    pub zones: [RawAwbZone; AWB_STATS_NUM_ZONES],
    /// Floating regions.
    pub floating: [RawAwbZone; FLOATING_STATS_NUM_ZONES],
}

/// `pisp_cdaf_statistics`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct RawCdafStatistics {
    /// Figures of merit, row-major.
    pub foms: [u64; CDAF_STATS_NUM_FOMS],
    /// Floating regions.
    pub floating: [u64; FLOATING_STATS_NUM_ZONES],
}

/// `pisp_statistics`: the whole statistics buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct RawStatistics {
    /// White balance.
    pub awb: RawAwbStatistics,
    /// Exposure.
    pub agc: RawAgcStatistics,
    /// Focus.
    pub cdaf: RawCdafStatistics,
}
