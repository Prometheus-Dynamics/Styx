//! `pisp_fe_config.h`: the front end configuration written to the `rp1-cfe-fe_config` node
//! (`V4L2_META_FMT_RPI_FE_CFG`), one buffer per frame.

use bytemuck::{Pod, Zeroable};

use super::common::{BlaConfig, CompressConfig, DecompressConfig, ImageFormatConfig};
use super::stats::AGC_STATS_NUM_ZONES;
use super::stats::FLOATING_STATS_NUM_ZONES;

/// Number of front end output branches.
pub const FE_NUM_OUTPUTS: usize = 2;
/// Decompanding LUT entries.
pub const FE_DECOMPAND_LUT_SIZE: usize = 65;
/// Front end LSC LUT entries.
pub const FE_LSC_LUT_SIZE: usize = 16;
/// CDAF weights.
pub const FE_CDAF_NUM_WEIGHTS: usize = 8;

/// `enum pisp_fe_enable`: block enables (also used as "dirty" flags).
pub mod fe_enable {
    /// Input.
    pub const INPUT: u32 = 0x00_0001;
    /// Decompression.
    pub const DECOMPRESS: u32 = 0x00_0002;
    /// Decompanding LUT.
    pub const DECOMPAND: u32 = 0x00_0004;
    /// Black level adjustment (image path).
    pub const BLA: u32 = 0x00_0008;
    /// Defective pixel correction.
    pub const DPC: u32 = 0x00_0010;
    /// Statistics crop.
    pub const STATS_CROP: u32 = 0x00_0020;
    /// Statistics 2x decimation.
    pub const DECIMATE: u32 = 0x00_0040;
    /// Black level correction (statistics path).
    pub const BLC: u32 = 0x00_0080;
    /// Focus statistics.
    pub const CDAF_STATS: u32 = 0x00_0100;
    /// White balance statistics.
    pub const AWB_STATS: u32 = 0x00_0200;
    /// RGB to Y conversion for AGC statistics.
    pub const RGBY: u32 = 0x00_0400;
    /// Lens shading (statistics path).
    pub const LSC: u32 = 0x00_0800;
    /// Exposure statistics.
    pub const AGC_STATS: u32 = 0x00_1000;
    /// Output 0 crop.
    pub const CROP0: u32 = 0x01_0000;
    /// Output 0 downscale.
    pub const DOWNSCALE0: u32 = 0x02_0000;
    /// Output 0 compression.
    pub const COMPRESS0: u32 = 0x04_0000;
    /// Output 0.
    pub const OUTPUT0: u32 = 0x08_0000;
    /// Output 1 crop.
    pub const CROP1: u32 = 0x10_0000;
    /// Output 1 downscale.
    pub const DOWNSCALE1: u32 = 0x20_0000;
    /// Output 1 compression.
    pub const COMPRESS1: u32 = 0x40_0000;
    /// Output 1.
    pub const OUTPUT1: u32 = 0x80_0000;

    /// `CROP0` for branch `i`.
    pub const fn crop(i: usize) -> u32 {
        CROP0 << (4 * i)
    }
    /// `DOWNSCALE0` for branch `i`.
    pub const fn downscale(i: usize) -> u32 {
        DOWNSCALE0 << (4 * i)
    }
    /// `COMPRESS0` for branch `i`.
    pub const fn compress(i: usize) -> u32 {
        COMPRESS0 << (4 * i)
    }
    /// `OUTPUT0` for branch `i`.
    pub const fn output(i: usize) -> u32 {
        OUTPUT0 << (4 * i)
    }
}

/// `enum pisp_fe_dirty`: extra dirty flags.
pub mod fe_dirty {
    /// Global block.
    pub const GLOBAL: u32 = 0x0001;
    /// Floating statistics regions.
    pub const FLOATING: u32 = 0x0002;
    /// Output AXI settings.
    pub const OUTPUT_AXI: u32 = 0x0004;
}

/// `struct pisp_fe_global_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeGlobalConfig {
    /// [`fe_enable`] bits.
    pub enables: u32,
    /// [`super::BayerOrder`] value.
    pub bayer_order: u8,
    /// Padding.
    pub pad: [u8; 3],
}

/// `struct pisp_fe_input_axi_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeInputAxiConfig {
    /// Burst length minus one OR'd with flags.
    pub maxlen_flags: u8,
    /// `{ prot[2:0], cache[3:0] }`.
    pub cache_prot: u8,
    /// QoS (4 bits).
    pub qos: u16,
}

/// `struct pisp_fe_output_axi_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeOutputAxiConfig {
    /// Burst length minus one OR'd with flags.
    pub maxlen_flags: u8,
    /// `{ prot[2:0], cache[3:0] }`.
    pub cache_prot: u8,
    /// QoS (4 fields of 4 bits for the panic levels).
    pub qos: u16,
    /// Output FIFO panic threshold.
    pub thresh: u16,
    /// Output FIFO statistics throttle threshold.
    pub throttle: u16,
}

/// `struct pisp_fe_input_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeInputConfig {
    /// 1: input streams from CSI-2 (the only mode the driver supports).
    pub streaming: u8,
    /// Padding.
    pub pad: [u8; 3],
    /// Input image format.
    pub format: ImageFormatConfig,
    /// AXI reader settings (memory input only).
    pub axi: FeInputAxiConfig,
    /// Extra cycles before each burst request.
    pub holdoff: u8,
    /// Padding.
    pub pad2: [u8; 3],
}

/// `struct pisp_fe_output_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeOutputConfig {
    /// Output image format.
    pub format: ImageFormatConfig,
    /// Line interrupt (set by the driver).
    pub ilines: u16,
    /// Padding.
    pub pad: [u8; 2],
}

/// `struct pisp_fe_input_buffer_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeInputBufferConfig {
    /// Address low 32 bits.
    pub addr_lo: u32,
    /// Address high 32 bits.
    pub addr_hi: u32,
    /// Frame id.
    pub frame_id: u16,
    /// Padding.
    pub pad: u16,
}

/// `struct pisp_fe_decompand_config`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeDecompandConfig {
    /// Piecewise-linear LUT.
    pub lut: [u16; FE_DECOMPAND_LUT_SIZE],
    /// Padding.
    pub pad: u16,
}

/// `struct pisp_fe_dpc_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeDpcConfig {
    /// Coefficient for the darkest neighbour.
    pub coeff_level: u8,
    /// Coefficient for the range.
    pub coeff_range: u8,
    /// Second range coefficient.
    pub coeff_range2: u8,
    /// `FOLDBACK` (1) and `VFLAG` (2).
    pub flags: u8,
}

/// `struct pisp_fe_lsc_config` (radial lens shading for the statistics).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeLscConfig {
    /// Radius-squared shift.
    pub shift: u8,
    /// Padding.
    pub pad0: u8,
    /// Radius-squared scale.
    pub scale: u16,
    /// Centre x.
    pub centre_x: u16,
    /// Centre y.
    pub centre_y: u16,
    /// Gains along the radius.
    pub lut: [u16; FE_LSC_LUT_SIZE],
}

/// `struct pisp_fe_rgby_config`: the RGB to Y weights for AGC statistics (4.10 fixed point).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeRgbyConfig {
    /// Red weight.
    pub gain_r: u16,
    /// Green weight.
    pub gain_g: u16,
    /// Blue weight.
    pub gain_b: u16,
    /// Use max(R, G, B) instead.
    pub maxflag: u8,
    /// Padding.
    pub pad: u8,
}

/// `struct pisp_fe_agc_stats_config`: the 16x16 zone grid, weights and row sums.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeAgcStatsConfig {
    /// Grid offset x.
    pub offset_x: u16,
    /// Grid offset y.
    pub offset_y: u16,
    /// Zone width.
    pub size_x: u16,
    /// Zone height.
    pub size_y: u16,
    /// 4-bit weights, two zones per byte (low nibble first), 8 bytes per row.
    pub weights: [u8; AGC_STATS_NUM_ZONES / 2],
    /// Row sums region offset x.
    pub row_offset_x: u16,
    /// Row sums region offset y.
    pub row_offset_y: u16,
    /// Row sums region width.
    pub row_size_x: u16,
    /// Rows per row sum.
    pub row_size_y: u16,
    /// Right shift of each row sum.
    pub row_shift: u8,
    /// Right shift of the floating region sums.
    pub float_shift: u8,
    /// Padding.
    pub pad1: [u8; 2],
}

/// `struct pisp_fe_awb_stats_config`: the 32x32 zone grid and pixel value limits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeAwbStatsConfig {
    /// Grid offset x.
    pub offset_x: u16,
    /// Grid offset y.
    pub offset_y: u16,
    /// Zone width.
    pub size_x: u16,
    /// Zone height.
    pub size_y: u16,
    /// Right shift of the sums.
    pub shift: u8,
    /// Padding.
    pub pad: [u8; 3],
    /// Pixels are counted only when every channel is within its `[lo, hi]`.
    pub r_lo: u16,
    /// Red upper limit.
    pub r_hi: u16,
    /// Green lower limit.
    pub g_lo: u16,
    /// Green upper limit.
    pub g_hi: u16,
    /// Blue lower limit.
    pub b_lo: u16,
    /// Blue upper limit.
    pub b_hi: u16,
}

/// `struct pisp_fe_floating_stats_region`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeFloatingStatsRegion {
    /// Offset x.
    pub offset_x: u16,
    /// Offset y.
    pub offset_y: u16,
    /// Width.
    pub size_x: u16,
    /// Height.
    pub size_y: u16,
}

/// `struct pisp_fe_floating_stats_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeFloatingStatsConfig {
    /// The regions.
    pub regions: [FeFloatingStatsRegion; FLOATING_STATS_NUM_ZONES],
}

/// `struct pisp_fe_cdaf_stats_config`: the 8x8 focus grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeCdafStatsConfig {
    /// Noise constant.
    pub noise_constant: u16,
    /// Noise slope.
    pub noise_slope: u16,
    /// Grid offset x.
    pub offset_x: u16,
    /// Grid offset y.
    pub offset_y: u16,
    /// Zone width.
    pub size_x: u16,
    /// Zone height.
    pub size_y: u16,
    /// Horizontal skip.
    pub skip_x: u16,
    /// Vertical skip.
    pub skip_y: u16,
    /// Channel selection and weights.
    pub mode: u32,
}

/// `struct pisp_fe_stats_buffer_config` (address filled in by the driver).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeStatsBufferConfig {
    /// Address low 32 bits.
    pub addr_lo: u32,
    /// Address high 32 bits.
    pub addr_hi: u32,
}

/// `struct pisp_fe_crop_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeCropConfig {
    /// Offset x.
    pub offset_x: u16,
    /// Offset y.
    pub offset_y: u16,
    /// Width.
    pub width: u16,
    /// Height.
    pub height: u16,
}

/// `enum pisp_fe_downscale_flags`: downscale the four Bayer channels independently.
pub const FE_DOWNSCALE_BAYER: u8 = 1;
/// `enum pisp_fe_downscale_flags`: bin without keeping the spatial relationship.
pub const FE_DOWNSCALE_BIN: u8 = 2;

/// `struct pisp_fe_downscale_config`: `xout/xin` by `yout/yin` scaling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeDownscaleConfig {
    /// Horizontal input count.
    pub xin: u8,
    /// Horizontal output count.
    pub xout: u8,
    /// Vertical input count.
    pub yin: u8,
    /// Vertical output count.
    pub yout: u8,
    /// `FE_DOWNSCALE_*`.
    pub flags: u8,
    /// Padding.
    pub pad: [u8; 3],
    /// Output width.
    pub output_width: u16,
    /// Output height.
    pub output_height: u16,
}

/// `struct pisp_fe_output_buffer_config` (address filled in by the driver).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeOutputBufferConfig {
    /// Address low 32 bits.
    pub addr_lo: u32,
    /// Address high 32 bits.
    pub addr_hi: u32,
}

/// `struct pisp_fe_output_branch_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeOutputBranchConfig {
    /// Crop.
    pub crop: FeCropConfig,
    /// Downscale.
    pub downscale: FeDownscaleConfig,
    /// Compression.
    pub compress: CompressConfig,
    /// Output format.
    pub output: FeOutputConfig,
    /// Padding.
    pub pad: u32,
}

/// `struct pisp_fe_config`: everything the front end needs for one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct FeConfig {
    /// Statistics buffer (driver).
    pub stats_buffer: FeStatsBufferConfig,
    /// Output buffers (driver).
    pub output_buffer: [FeOutputBufferConfig; FE_NUM_OUTPUTS],
    /// Input buffer (memory input; unused when streaming).
    pub input_buffer: FeInputBufferConfig,
    /// Enables and Bayer order.
    pub global: FeGlobalConfig,
    /// Input.
    pub input: FeInputConfig,
    /// Decompression.
    pub decompress: DecompressConfig,
    /// Decompanding.
    pub decompand: FeDecompandConfig,
    /// Black level (image path).
    pub bla: BlaConfig,
    /// Defective pixel correction.
    pub dpc: FeDpcConfig,
    /// Statistics crop.
    pub stats_crop: FeCropConfig,
    /// Reserved for decimation.
    pub spare1: u32,
    /// Black level (statistics path).
    pub blc: BlaConfig,
    /// RGB to Y.
    pub rgby: FeRgbyConfig,
    /// Lens shading (statistics path).
    pub lsc: FeLscConfig,
    /// AGC statistics.
    pub agc_stats: FeAgcStatsConfig,
    /// AWB statistics.
    pub awb_stats: FeAwbStatsConfig,
    /// Focus statistics.
    pub cdaf_stats: FeCdafStatsConfig,
    /// Floating statistics regions.
    pub floating_stats: FeFloatingStatsConfig,
    /// Output AXI.
    pub output_axi: FeOutputAxiConfig,
    /// Output branches.
    pub ch: [FeOutputBranchConfig; FE_NUM_OUTPUTS],
    /// Blocks to (re)write, [`fe_enable`] bits (the driver always writes some).
    pub dirty_flags: u32,
    /// Extra dirty flags, [`fe_dirty`] bits.
    pub dirty_flags_extra: u32,
}

impl Default for FeConfig {
    fn default() -> Self {
        Zeroable::zeroed()
    }
}

impl FeConfig {
    /// The config as the bytes written to the config node.
    pub fn as_bytes(&self) -> &[u8] {
        bytemuck::bytes_of(self)
    }
}
