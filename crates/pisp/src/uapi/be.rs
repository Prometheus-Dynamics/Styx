//! `pisp_be_config.h`: back end processing blocks. The header declares every structure
//! `packed`; all of them happen to have no implicit padding, so `#[repr(C)]` gives the same
//! layout (checked in `layout.rs` against offsets generated from the header).

use bytemuck::{Pod, Zeroable};

/// Byte alignment for inputs.
pub const BE_INPUT_ALIGN: u32 = 4;
/// Alignment for compressed inputs (pixels).
pub const BE_COMPRESSED_ALIGN: u32 = 8;
/// Minimum output byte alignment.
pub const BE_OUTPUT_MIN_ALIGN: u32 = 16;
/// Preferred output byte alignment.
pub const BE_OUTPUT_MAX_ALIGN: u32 = 64;
/// Minimum tile width anywhere in the pipeline.
pub const BE_MIN_TILE_WIDTH: u32 = 16;
/// Minimum tile height anywhere in the pipeline.
pub const BE_MIN_TILE_HEIGHT: u32 = 16;
/// Output branches.
pub const BE_NUM_OUTPUTS: usize = 2;
/// Maximum tiles per job.
pub const BE_NUM_TILES: usize = 64;

/// `enum pisp_be_bayer_enable`.
pub mod bayer_enable {
    /// Bayer input.
    pub const INPUT: u32 = 0x00_0001;
    /// Input decompression.
    pub const DECOMPRESS: u32 = 0x00_0002;
    /// Defective pixel correction.
    pub const DPC: u32 = 0x00_0004;
    /// Green equalisation.
    pub const GEQ: u32 = 0x00_0008;
    /// Temporal denoise input.
    pub const TDN_INPUT: u32 = 0x00_0010;
    /// TDN input decompression.
    pub const TDN_DECOMPRESS: u32 = 0x00_0020;
    /// Temporal denoise.
    pub const TDN: u32 = 0x00_0040;
    /// TDN output compression.
    pub const TDN_COMPRESS: u32 = 0x00_0080;
    /// TDN output.
    pub const TDN_OUTPUT: u32 = 0x00_0100;
    /// Spatial denoise.
    pub const SDN: u32 = 0x00_0200;
    /// Black level correction.
    pub const BLC: u32 = 0x00_0400;
    /// HDR stitch input.
    pub const STITCH_INPUT: u32 = 0x00_0800;
    /// Stitch input decompression.
    pub const STITCH_DECOMPRESS: u32 = 0x00_1000;
    /// HDR stitch.
    pub const STITCH: u32 = 0x00_2000;
    /// Stitch output compression.
    pub const STITCH_COMPRESS: u32 = 0x00_4000;
    /// Stitch output.
    pub const STITCH_OUTPUT: u32 = 0x00_8000;
    /// White balance gains.
    pub const WBG: u32 = 0x01_0000;
    /// Colour denoise.
    pub const CDN: u32 = 0x02_0000;
    /// Lens shading correction.
    pub const LSC: u32 = 0x04_0000;
    /// Tone mapping.
    pub const TONEMAP: u32 = 0x08_0000;
    /// Chromatic aberration correction.
    pub const CAC: u32 = 0x10_0000;
    /// Debinning.
    pub const DEBIN: u32 = 0x20_0000;
    /// Demosaic.
    pub const DEMOSAIC: u32 = 0x40_0000;
}

/// `enum pisp_be_rgb_enable`.
pub mod rgb_enable {
    /// RGB input (instead of Bayer).
    pub const INPUT: u32 = 0x00_0001;
    /// Colour correction matrix.
    pub const CCM: u32 = 0x00_0002;
    /// Saturation control.
    pub const SAT_CONTROL: u32 = 0x00_0004;
    /// RGB to YCbCr.
    pub const YCBCR: u32 = 0x00_0008;
    /// False colour suppression.
    pub const FALSE_COLOUR: u32 = 0x00_0010;
    /// Sharpening.
    pub const SHARPEN: u32 = 0x00_0020;
    /// YCbCr back to RGB.
    pub const YCBCR_INVERSE: u32 = 0x00_0080;
    /// Gamma.
    pub const GAMMA: u32 = 0x00_0100;
    /// Output 0 colour space conversion.
    pub const CSC0: u32 = 0x00_0200;
    /// Output 1 colour space conversion.
    pub const CSC1: u32 = 0x00_0400;
    /// Output 0 downscaler.
    pub const DOWNSCALE0: u32 = 0x00_1000;
    /// Output 1 downscaler.
    pub const DOWNSCALE1: u32 = 0x00_2000;
    /// Output 0 resampler.
    pub const RESAMPLE0: u32 = 0x00_8000;
    /// Output 1 resampler.
    pub const RESAMPLE1: u32 = 0x01_0000;
    /// Output 0.
    pub const OUTPUT0: u32 = 0x04_0000;
    /// Output 1.
    pub const OUTPUT1: u32 = 0x08_0000;
    /// HOG output.
    pub const HOG: u32 = 0x20_0000;

    /// `CSC0` for branch `i`.
    pub const fn csc(i: usize) -> u32 {
        CSC0 << i
    }
    /// `DOWNSCALE0` for branch `i`.
    pub const fn downscale(i: usize) -> u32 {
        DOWNSCALE0 << i
    }
    /// `RESAMPLE0` for branch `i`.
    pub const fn resample(i: usize) -> u32 {
        RESAMPLE0 << i
    }
    /// `OUTPUT0` for branch `i`.
    pub const fn output(i: usize) -> u32 {
        OUTPUT0 << i
    }
}

/// `enum pisp_be_dirty`.
pub mod be_dirty {
    /// Global block.
    pub const GLOBAL: u32 = 0x0001;
    /// Sharpen/false colour combine.
    pub const SH_FC_COMBINE: u32 = 0x0002;
    /// Crops.
    pub const CROP: u32 = 0x0004;
}

/// `struct pisp_be_global_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeGlobalConfig {
    /// [`bayer_enable`] bits.
    pub bayer_enables: u32,
    /// [`rgb_enable`] bits.
    pub rgb_enables: u32,
    /// [`super::BayerOrder`] value.
    pub bayer_order: u8,
    /// Padding.
    pub pad: [u8; 3],
}

/// `struct pisp_be_input_buffer_config` (addresses are the driver's).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeInputBufferConfig {
    /// Low/high address per plane.
    pub addr: [[u32; 2]; 3],
}

/// `struct pisp_be_dpc_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeDpcConfig {
    /// Coefficient for the darkest neighbour.
    pub coeff_level: u8,
    /// Coefficient for the range.
    pub coeff_range: u8,
    /// Padding.
    pub pad: u8,
    /// `FOLDBACK` (1).
    pub flags: u8,
}

/// `struct pisp_be_geq_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeGeqConfig {
    /// Threshold offset.
    pub offset: u16,
    /// Bit 15 "sharper", bits 0..9 slope.
    pub slope_sharper: u16,
    /// Minimum threshold.
    pub min: u16,
    /// Maximum threshold.
    pub max: u16,
}

/// `struct pisp_be_tdn_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeTdnConfig {
    /// Black level.
    pub black_level: u16,
    /// Long-term average ratio.
    pub ratio: u16,
    /// Noise constant.
    pub noise_constant: u16,
    /// Noise slope.
    pub noise_slope: u16,
    /// Threshold.
    pub threshold: u16,
    /// Reset (no previous frame).
    pub reset: u8,
    /// Padding.
    pub pad: u8,
}

/// `struct pisp_be_sdn_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeSdnConfig {
    /// Black level.
    pub black_level: u16,
    /// Proportion of the original mixed back in.
    pub leakage: u8,
    /// Padding.
    pub pad: u8,
    /// Noise constant.
    pub noise_constant: u16,
    /// Noise slope.
    pub noise_slope: u16,
    /// Second noise constant.
    pub noise_constant2: u16,
    /// Second noise slope.
    pub noise_slope2: u16,
}

/// `struct pisp_be_stitch_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeStitchConfig {
    /// Low threshold.
    pub threshold_lo: u16,
    /// log2 of the high-low threshold difference.
    pub threshold_diff_power: u8,
    /// Padding.
    pub pad: u8,
    /// Exposure ratio; bit 15 set when the streaming input is the long exposure.
    pub exposure_ratio: u16,
    /// Motion threshold.
    pub motion_threshold_256: u8,
    /// Its reciprocal.
    pub motion_threshold_recip: u8,
}

/// `struct pisp_be_cdn_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeCdnConfig {
    /// Noise threshold.
    pub thresh: u16,
    /// IIR strength.
    pub iir_strength: u8,
    /// Proportion of the change assigned to G.
    pub g_adjust: u8,
}

/// LSC grid cells per side.
pub const BE_LSC_GRID_SIZE: usize = 32;
/// LSC LUT points per side.
pub const BE_LSC_LUT_SIZE: usize = BE_LSC_GRID_SIZE + 1;
/// LSC step precision.
pub const BE_LSC_STEP_PRECISION: u32 = 18;

/// `struct pisp_be_lsc_config`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeLscConfig {
    /// `(1 << 18) / cell width`.
    pub grid_step_x: u16,
    /// `(1 << 18) / cell height`.
    pub grid_step_y: u16,
    /// RGB gains jointly coded in 32 bits.
    pub lut_packed: [[u32; BE_LSC_LUT_SIZE]; BE_LSC_LUT_SIZE],
}

/// `struct pisp_be_lsc_extra` (libpisp-side, not in `pisp_be_config`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeLscExtra {
    /// Horizontal offset into the table.
    pub offset_x: u16,
    /// Vertical offset into the table.
    pub offset_y: u16,
}

/// CAC grid cells per side.
pub const BE_CAC_GRID_SIZE: usize = 8;
/// CAC LUT points per side.
pub const BE_CAC_LUT_SIZE: usize = BE_CAC_GRID_SIZE + 1;
/// CAC step precision.
pub const BE_CAC_STEP_PRECISION: u32 = 20;

/// `struct pisp_be_cac_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeCacConfig {
    /// `(1 << 20) / cell width`.
    pub grid_step_x: u16,
    /// `(1 << 20) / cell height`.
    pub grid_step_y: u16,
    /// `[y][x][rb][xy]` pixel shifts.
    pub lut: [[[[i8; 2]; 2]; BE_CAC_LUT_SIZE]; BE_CAC_LUT_SIZE],
}

/// `struct pisp_be_debin_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeDebinConfig {
    /// Filter coefficients.
    pub coeffs: [i8; 4],
    /// Horizontal enable.
    pub h_enable: i8,
    /// Vertical enable.
    pub v_enable: i8,
    /// Padding.
    pub pad: [i8; 2],
}

/// Tonemap LUT entries.
pub const BE_TONEMAP_LUT_SIZE: usize = 64;

/// `struct pisp_be_tonemap_config`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeTonemapConfig {
    /// Detail threshold constant.
    pub detail_constant: u16,
    /// Detail threshold slope.
    pub detail_slope: u16,
    /// IIR strength.
    pub iir_strength: u16,
    /// Strength.
    pub strength: u16,
    /// Curve.
    pub lut: [u32; BE_TONEMAP_LUT_SIZE],
}

/// `struct pisp_be_demosaic_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeDemosaicConfig {
    /// Use the other channels to sharpen.
    pub sharper: u8,
    /// Built-in false colour suppression mode.
    pub fc_mode: u8,
    /// Padding.
    pub pad: [u8; 2],
}

/// `struct pisp_be_ccm_config`: a 3x3 matrix (s4.10) plus offsets; also used for YCbCr
/// conversions and the output CSCs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeCcmConfig {
    /// Row-major coefficients, 1024 = 1.0.
    pub coeffs: [i16; 9],
    /// Padding.
    pub pad: [u8; 2],
    /// Offsets added after the matrix (in 16.10 units: `x << 10` for a 16-bit value `x`).
    pub offsets: [i32; 3],
}

/// `struct pisp_be_sat_control_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeSatControlConfig {
    /// Red left shift.
    pub shift_r: u8,
    /// Green left shift.
    pub shift_g: u8,
    /// Blue left shift.
    pub shift_b: u8,
    /// Padding.
    pub pad: u8,
}

/// `struct pisp_be_false_colour_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeFalseColourConfig {
    /// Neighbour distance, 1 or 2.
    pub distance: u8,
    /// Padding.
    pub pad: [u8; 3],
}

/// Sharpening kernel side.
pub const BE_SHARPEN_SIZE: usize = 5;
/// Sharpening response function points.
pub const BE_SHARPEN_FUNC_NUM_POINTS: usize = 9;

/// One of the five sharpening filters (kernel and threshold) as laid out in
/// `struct pisp_be_sharpen_config`.
pub type SharpenKernel = [i8; BE_SHARPEN_SIZE * BE_SHARPEN_SIZE];

/// `struct pisp_be_sharpen_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeSharpenConfig {
    /// Filter 0 kernel.
    pub kernel0: SharpenKernel,
    /// Padding.
    pub pad0: [i8; 3],
    /// Filter 1 kernel.
    pub kernel1: SharpenKernel,
    /// Padding.
    pub pad1: [i8; 3],
    /// Filter 2 kernel.
    pub kernel2: SharpenKernel,
    /// Padding.
    pub pad2: [i8; 3],
    /// Filter 3 kernel.
    pub kernel3: SharpenKernel,
    /// Padding.
    pub pad3: [i8; 3],
    /// Filter 4 kernel.
    pub kernel4: SharpenKernel,
    /// Padding.
    pub pad4: [i8; 3],
    /// `[offset, slope, scale, pad]` for filters 0..4.
    pub thresholds: [[u16; 4]; 5],
    /// Positive strength.
    pub positive_strength: u16,
    /// Positive pre-limit.
    pub positive_pre_limit: u16,
    /// Positive response function.
    pub positive_func: [u16; BE_SHARPEN_FUNC_NUM_POINTS],
    /// Positive limit.
    pub positive_limit: u16,
    /// Negative strength.
    pub negative_strength: u16,
    /// Negative pre-limit.
    pub negative_pre_limit: u16,
    /// Negative response function.
    pub negative_func: [u16; BE_SHARPEN_FUNC_NUM_POINTS],
    /// Negative limit.
    pub negative_limit: u16,
    /// Filter enable mask.
    pub enables: u8,
    /// White pixel filter mask.
    pub white: u8,
    /// Black pixel filter mask.
    pub black: u8,
    /// Grey pixel filter mask.
    pub grey: u8,
}

/// `struct pisp_be_sh_fc_combine_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeShFcCombineConfig {
    /// Y factor.
    pub y_factor: u8,
    /// Cb factor.
    pub c1_factor: u8,
    /// Cr factor.
    pub c2_factor: u8,
    /// Padding.
    pub pad: u8,
}

/// Gamma LUT entries.
pub const BE_GAMMA_LUT_SIZE: usize = 64;

/// `struct pisp_be_gamma_config`: each entry is `slope << 16 | value`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeGammaConfig {
    /// The curve.
    pub lut: [u32; BE_GAMMA_LUT_SIZE],
}

/// `struct pisp_be_crop_config` (libpisp side; the hardware sees per-tile crops).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeCropConfig {
    /// Offset x.
    pub offset_x: u16,
    /// Offset y.
    pub offset_y: u16,
    /// Width.
    pub width: u16,
    /// Height.
    pub height: u16,
}

/// Resampler filter coefficients (16 phases x 6 taps).
pub const BE_RESAMPLE_FILTER_SIZE: usize = 96;

/// `struct pisp_be_resample_config`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeResampleConfig {
    /// Horizontal scale factor (4.12).
    pub scale_factor_h: u16,
    /// Vertical scale factor (4.12).
    pub scale_factor_v: u16,
    /// Filter coefficients (s.10).
    pub coef: [i16; BE_RESAMPLE_FILTER_SIZE],
}

/// `struct pisp_be_resample_extra` (libpisp side).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeResampleExtra {
    /// Output width.
    pub scaled_width: u16,
    /// Output height.
    pub scaled_height: u16,
    /// Initial horizontal phase per plane.
    pub initial_phase_h: [i16; 3],
    /// Initial vertical phase per plane.
    pub initial_phase_v: [i16; 3],
}

/// `struct pisp_be_downscale_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeDownscaleConfig {
    /// Horizontal scale factor (4.12).
    pub scale_factor_h: u16,
    /// Vertical scale factor (4.12).
    pub scale_factor_v: u16,
    /// Horizontal reciprocal.
    pub scale_recip_h: u16,
    /// Vertical reciprocal.
    pub scale_recip_v: u16,
}

/// `struct pisp_be_downscale_extra` (libpisp side).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeDownscaleExtra {
    /// Output width.
    pub scaled_width: u16,
    /// Output height.
    pub scaled_height: u16,
}

/// `struct pisp_be_hog_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeHogConfig {
    /// Signed gradients.
    pub compute_signed: u8,
    /// Channel mix.
    pub channel_mix: [u8; 3],
    /// Stride.
    pub stride: u32,
}

/// `struct pisp_be_axi_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeAxiConfig {
    /// Read QoS.
    pub r_qos: u8,
    /// Read `{ prot[2:0], cache[3:0] }`.
    pub r_cache_prot: u8,
    /// Write QoS.
    pub w_qos: u8,
    /// Write `{ prot[2:0], cache[3:0] }`.
    pub w_cache_prot: u8,
}

/// `enum pisp_be_transform`: horizontal flip.
pub const BE_TRANSFORM_HFLIP: u8 = 0x1;
/// `enum pisp_be_transform`: vertical flip.
pub const BE_TRANSFORM_VFLIP: u8 = 0x2;

/// `struct pisp_be_output_format_config`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BeOutputFormatConfig {
    /// Output image format.
    pub image: super::ImageFormatConfig,
    /// `BE_TRANSFORM_*`.
    pub transform: u8,
    /// Padding.
    pub pad: [u8; 3],
    /// Low clip of the first channel.
    pub lo: u16,
    /// High clip of the first channel (0 means no clipping, set to 65535 by the builder).
    pub hi: u16,
    /// Low clip of the other channels.
    pub lo2: u16,
    /// High clip of the other channels.
    pub hi2: u16,
}
