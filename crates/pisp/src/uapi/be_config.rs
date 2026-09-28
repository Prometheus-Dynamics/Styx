//! `struct pisp_be_config`, `struct pisp_tile` and `struct pisp_be_tiles_config`: what is
//! written to the `pispbe-config` node (`V4L2_META_FMT_RPI_BE_CFG`), one buffer per job.

use bytemuck::{Pod, Zeroable};

use super::be::*;
use super::common::{BlaConfig, CompressConfig, DecompressConfig, ImageFormatConfig, WbgConfig};

/// `struct pisp_be_config`: the processing configuration (buffer addresses and the effective
/// enables are the driver's).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeConfig {
    /// Formerly the buffer addresses; ignored.
    pub pad0: [u8; 112],
    /// Enables and Bayer order.
    pub global: BeGlobalConfig,
    /// Main input format.
    pub input_format: ImageFormatConfig,
    /// Input decompression.
    pub decompress: DecompressConfig,
    /// Defective pixel correction.
    pub dpc: BeDpcConfig,
    /// Green equalisation.
    pub geq: BeGeqConfig,
    /// TDN input format.
    pub tdn_input_format: ImageFormatConfig,
    /// TDN input decompression.
    pub tdn_decompress: DecompressConfig,
    /// Temporal denoise.
    pub tdn: BeTdnConfig,
    /// TDN output compression.
    pub tdn_compress: CompressConfig,
    /// TDN output format.
    pub tdn_output_format: ImageFormatConfig,
    /// Spatial denoise.
    pub sdn: BeSdnConfig,
    /// Black level correction.
    pub blc: BlaConfig,
    /// Stitch output compression.
    pub stitch_compress: CompressConfig,
    /// Stitch output format.
    pub stitch_output_format: ImageFormatConfig,
    /// Stitch input format.
    pub stitch_input_format: ImageFormatConfig,
    /// Stitch input decompression.
    pub stitch_decompress: DecompressConfig,
    /// HDR stitch.
    pub stitch: BeStitchConfig,
    /// Lens shading.
    pub lsc: BeLscConfig,
    /// White balance gains.
    pub wbg: WbgConfig,
    /// Colour denoise.
    pub cdn: BeCdnConfig,
    /// Chromatic aberration.
    pub cac: BeCacConfig,
    /// Debinning.
    pub debin: BeDebinConfig,
    /// Tone mapping.
    pub tonemap: BeTonemapConfig,
    /// Demosaic.
    pub demosaic: BeDemosaicConfig,
    /// Colour correction matrix.
    pub ccm: BeCcmConfig,
    /// Saturation control.
    pub sat_control: BeSatControlConfig,
    /// RGB to YCbCr.
    pub ycbcr: BeCcmConfig,
    /// Sharpening.
    pub sharpen: BeSharpenConfig,
    /// False colour.
    pub false_colour: BeFalseColourConfig,
    /// Sharpen/false colour combine.
    pub sh_fc_combine: BeShFcCombineConfig,
    /// YCbCr to RGB.
    pub ycbcr_inverse: BeCcmConfig,
    /// Gamma.
    pub gamma: BeGammaConfig,
    /// Per-output colour space conversion.
    pub csc: [BeCcmConfig; BE_NUM_OUTPUTS],
    /// Per-output downscalers.
    pub downscale: [BeDownscaleConfig; BE_NUM_OUTPUTS],
    /// Per-output resamplers.
    pub resample: [BeResampleConfig; BE_NUM_OUTPUTS],
    /// Per-output formats.
    pub output_format: [BeOutputFormatConfig; BE_NUM_OUTPUTS],
    /// HOG.
    pub hog: BeHogConfig,
    /// AXI.
    pub axi: BeAxiConfig,
    /// Reserved.
    pub pad1: [u8; 84],
}

/// `enum pisp_tile_edge`: left edge.
pub const TILE_LEFT_EDGE: u8 = 1 << 0;
/// Right edge.
pub const TILE_RIGHT_EDGE: u8 = 1 << 1;
/// Top edge.
pub const TILE_TOP_EDGE: u8 = 1 << 2;
/// Bottom edge.
pub const TILE_BOTTOM_EDGE: u8 = 1 << 3;

/// `struct pisp_tile`: one tile's input window, crops, scaler phases and output window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct Tile {
    /// `TILE_*_EDGE` bits.
    pub edge: u8,
    /// Padding.
    pub pad0: [u8; 3],
    /// Byte offset of the tile in the input (plane 0).
    pub input_addr_offset: u32,
    /// Byte offset of the tile in the input (planes 1 and 2).
    pub input_addr_offset2: u32,
    /// Input x.
    pub input_offset_x: u16,
    /// Input y.
    pub input_offset_y: u16,
    /// Input width.
    pub input_width: u16,
    /// Input height.
    pub input_height: u16,
    /// TDN input byte offset.
    pub tdn_input_addr_offset: u32,
    /// TDN output byte offset.
    pub tdn_output_addr_offset: u32,
    /// Stitch input byte offset.
    pub stitch_input_addr_offset: u32,
    /// Stitch output byte offset.
    pub stitch_output_addr_offset: u32,
    /// LSC grid offset x.
    pub lsc_grid_offset_x: u32,
    /// LSC grid offset y.
    pub lsc_grid_offset_y: u32,
    /// CAC grid offset x.
    pub cac_grid_offset_x: u32,
    /// CAC grid offset y.
    pub cac_grid_offset_y: u32,
    /// Pixels cropped at the left, per output.
    pub crop_x_start: [u16; BE_NUM_OUTPUTS],
    /// Pixels cropped at the right, per output.
    pub crop_x_end: [u16; BE_NUM_OUTPUTS],
    /// Rows cropped at the top, per output.
    pub crop_y_start: [u16; BE_NUM_OUTPUTS],
    /// Rows cropped at the bottom, per output.
    pub crop_y_end: [u16; BE_NUM_OUTPUTS],
    /// Downscaler x phase, planes then branches.
    pub downscale_phase_x: [u16; 3 * BE_NUM_OUTPUTS],
    /// Downscaler y phase, planes then branches.
    pub downscale_phase_y: [u16; 3 * BE_NUM_OUTPUTS],
    /// Resampler input width.
    pub resample_in_width: [u16; BE_NUM_OUTPUTS],
    /// Resampler input height.
    pub resample_in_height: [u16; BE_NUM_OUTPUTS],
    /// Resampler x phase, planes then branches.
    pub resample_phase_x: [u16; 3 * BE_NUM_OUTPUTS],
    /// Resampler y phase, planes then branches.
    pub resample_phase_y: [u16; 3 * BE_NUM_OUTPUTS],
    /// Output x.
    pub output_offset_x: [u16; BE_NUM_OUTPUTS],
    /// Output y.
    pub output_offset_y: [u16; BE_NUM_OUTPUTS],
    /// Output width.
    pub output_width: [u16; BE_NUM_OUTPUTS],
    /// Output height.
    pub output_height: [u16; BE_NUM_OUTPUTS],
    /// Output byte offset (plane 0).
    pub output_addr_offset: [u32; BE_NUM_OUTPUTS],
    /// Output byte offset (planes 1 and 2).
    pub output_addr_offset2: [u32; BE_NUM_OUTPUTS],
    /// HOG output byte offset.
    pub output_hog_addr_offset: u32,
}

/// `struct pisp_be_tiles_config`: the config node's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeTilesConfig {
    /// Processing configuration.
    pub config: BeConfig,
    /// Tiles (the first `num_tiles` are used).
    pub tiles: [Tile; BE_NUM_TILES],
    /// Number of tiles.
    pub num_tiles: u32,
}

impl Default for BeConfig {
    fn default() -> Self {
        Zeroable::zeroed()
    }
}

impl Default for BeTilesConfig {
    fn default() -> Self {
        Zeroable::zeroed()
    }
}

impl BeTilesConfig {
    /// The config as the bytes written to the config node.
    pub fn as_bytes(&self) -> &[u8] {
        bytemuck::bytes_of(self)
    }
}
