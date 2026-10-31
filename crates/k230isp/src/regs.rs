//! The register map of the K230D's ISP8000: block bases, register offsets and the field
//! positions the builder and the statistics decoder use.
//!
//! Sources (all MIT, so usable here):
//! - `vvcam/isp/mrv_all_regs.h` (VeriSilicon, MIT; the Dream Chip MRV register structure).
//!   The block bases are the struct's gap arithmetic (`_notused_N[(0x... - 0x...) / 4]`); the
//!   member offsets are the struct's own. Field masks and shifts are its `*_MASK` / `*_SHIFT`
//!   defines.
//! - `vvcam/isp/mrv_all_bits.h` (VeriSilicon, MIT): the value ranges and the meaning of the
//!   fields, and the defaults (`0x100` = 1.0 for the AWB gains, `0x080` = 1.0 for the colour
//!   matrix).
//! - `vvcam/isp/vvcam_isp.h` and `vvcam_isp_hal.h` (VeriSilicon, MIT): the ioctls and the
//!   register-access struct of the vendor driver (see [`crate::device`]).
//!
//! Where the sources disagree, the struct's layout is used and the choice is named in the
//! constant's comment. Addresses are offsets within the register window the driver maps
//! (the `/dev/vvcam-isp.0` base).

// Block bases (mrv_all_regs.h, the gaps before each block's first member).

/// ISP core (`MRV_ISP_BASE`): global control, acquisition, AWB gains, colour matrix, gamma.
pub const ISP_BASE: u32 = 0x0400;
/// Main resizer (`MRV_MRSZ_BASE`).
pub const MRSZ_BASE: u32 = 0x0C00;
/// Self resizer (`MRV_SRSZ_BASE`).
pub const SRSZ_BASE: u32 = 0x1000;
/// Memory interface (`MRV_MI_BASE`).
pub const MI_BASE: u32 = 0x1400;
/// Auto focus measurement (`MRV_AFM_BASE`).
pub const AFM_BASE: u32 = 0x2000;
/// Lens shading correction (`MRV_LSC_BASE`).
pub const LSC_BASE: u32 = 0x2200;
/// Histogram (`MRV_HIST_BASE`).
pub const HIST_BASE: u32 = 0x2400;
/// Auto exposure measurement (`MRV_AE_BASE`): a 5x5 grid of luma means.
pub const AE_BASE: u32 = 0x2600;
/// Black level subtraction (`MRV_BLS_BASE`).
pub const BLS_BASE: u32 = 0x2700;
/// Denoising pre-filter (`MRV_DPF_BASE`).
pub const DPF_BASE: u32 = 0x2800;
/// Defective pixel correction (`MRV_DPCC_BASE`).
pub const DPCC_BASE: u32 = 0x2900;
/// Wide dynamic range (`MRV_WDR_BASE`).
pub const WDR_BASE: u32 = 0x2A00;
/// AWB measurement window (`ISP_AWB_BASE`, the gap before `awb_meas_mode` in
/// `mrv_all_regs.h`). The brief's block table has no entry for it.
pub const AWB_BASE: u32 = 0x2C00;
/// 3D noise reduction (the gap before it in `mrv_all_regs.h` is `_notused_44_3dnr`).
pub const DNR3_BASE: u32 = 0x3700;
/// Edge enhancement (the gap before it is `_notused_45`).
pub const EE_BASE: u32 = 0x3900;

// ISP core (mrv_all_regs.h, `MRV_ISP_BASE + offset`).

/// Global control (`isp_ctrl`): the update strobes and the gamma-out enable, see
/// [`CTRL_CFG_UPD`], [`CTRL_GEN_CFG_UPD`] and [`CTRL_GAMMA_OUT_ENABLE`].
pub const ISP_CTRL: u32 = ISP_BASE;
/// AWB properties (`isp_awb_prop`): bit 31 selects the RGB measurement mode
/// ([`AWB_PROP_RGB_MEAS`]).
pub const ISP_AWB_PROP: u32 = ISP_BASE + 0x110;
/// AWB white pixel count (`isp_awb_white_cnt`, read): [`AWB_WHITE_CNT_MASK`].
pub const ISP_AWB_WHITE_CNT: u32 = ISP_BASE + 0x140;
/// AWB measured means (`isp_awb_mean`, read): bits 23..16 green, 15..8 blue, 7..0 red in RGB
/// measurement mode (`mrv_all_regs.h` slices `AWB_MEAN_Y__G`, `AWB_MEAN_CB__B`, `AWB_MEAN_CR__R`).
pub const ISP_AWB_MEAN: u32 = ISP_BASE + 0x144;
/// AWB gain of green: bits 25..16 the green in a red line, bits 9..0 the green in a blue line.
pub const ISP_AWB_GAIN_G: u32 = ISP_BASE + 0x138;
/// AWB gain of red and blue: bits 25..16 red, bits 9..0 blue.
pub const ISP_AWB_GAIN_RB: u32 = ISP_BASE + 0x13C;
/// The colour conversion matrix's nine coefficients, row-major (`isp_cc_coeff_0` to `_8`),
/// one register each; see [`CC_COEFF_MASK`].
pub const ISP_CC_COEFF_0: u32 = ISP_BASE + 0x170;
/// Gamma-out segmentation (`isp_gamma_out_mode`): bit 0 selects equidistant segments
/// ([`GAMMA_OUT_EQU_SEGM`]).
pub const ISP_GAMMA_OUT_MODE: u32 = ISP_BASE + 0x1F4;
/// The output gamma's 17 points (`gamma_out_y_block_arr`), one register each, 10 bits
/// ([`GAMMA_Y_MASK`]). CONFLICT: `mrv_all_bits.h` puts `isp_gamma_out_y` at `0x3F0 + n*4`
/// (its `isp_ct_coeff` at `0x3A0`, the same relative numbering); the struct puts the array
/// right after `isp_gamma_out_mode`, at `0x1F8`. The struct is used; rkisp1's V10 layout puts
/// its gamma points at the same `0x1F8` (`RKISP1_CIF_ISP_GAMMA_OUT_Y_0_V10`). Unconfirmed.
pub const ISP_GAMMA_OUT_Y_0: u32 = ISP_BASE + 0x1F8;
/// Cross-talk offset, red / green / blue (`isp_ct_offset_r`, `_g`, `_b`), 12 bits signed
/// ([`CT_OFFSET_MASK`]). The header has no separate colour-matrix offset, so the builder's
/// colour offsets go here. Unconfirmed.
pub const ISP_CT_OFFSET_R: u32 = ISP_BASE + 0x248;
/// See [`ISP_CT_OFFSET_R`].
pub const ISP_CT_OFFSET_G: u32 = ISP_BASE + 0x24C;
/// See [`ISP_CT_OFFSET_R`].
pub const ISP_CT_OFFSET_B: u32 = ISP_BASE + 0x250;

/// `isp_ctrl` bit 9: immediate update of the internal shadow registers (write-only).
pub const CTRL_CFG_UPD: u32 = 1 << 9;
/// `isp_ctrl` bit 10: an update of the shadow registers at the next vertical sync
/// (write-only). The builder uses this one: 3A changes land on a frame boundary.
pub const CTRL_GEN_CFG_UPD: u32 = 1 << 10;
/// `isp_ctrl` bit 11: the output gamma is enabled.
pub const CTRL_GAMMA_OUT_ENABLE: u32 = 1 << 11;
/// `isp_awb_prop` bit 31: the RGB measurement mode (1), not the near-white YCbCr one (0).
pub const AWB_PROP_RGB_MEAS: u32 = 1 << 31;
/// `isp_awb_white_cnt`: 26 bits.
pub const AWB_WHITE_CNT_MASK: u32 = 0x03FF_FFFF;
/// AWB gain fields: 10 bits, 8 fractional (`100h = 1`).
pub const AWB_GAIN_MASK: u32 = 0x3FF;
/// The red (and green in a red line) gain field's shift in `isp_awb_gain_rb` / `_g`.
pub const AWB_GAIN_HI_SHIFT: u32 = 16;
/// Colour matrix coefficient fields: 9 bits, two's complement, 7 fractional (`080h = 1`).
pub const CC_COEFF_MASK: u32 = 0x1FF;
/// Gamma-out segmentation bit: equidistant (1), logarithmic like (0, the reset value).
pub const GAMMA_OUT_EQU_SEGM: u32 = 1;
/// Gamma-out point fields: 10 bits.
pub const GAMMA_Y_MASK: u32 = 0x3FF;
/// Cross-talk offset fields: 12 bits, two's complement.
pub const CT_OFFSET_MASK: u32 = 0xFFF;

// Lens shading (mrv_all_regs.h, `LSC_BASE + offset`).

/// Control (`isp_lsc_ctrl`): bit 0 enables the correction ([`LSC_EN`]).
pub const LSC_CTRL: u32 = LSC_BASE;
/// Table RAM address per colour (`isp_lsc_{r,gr,b,gb}_table_addr`); the data register that
/// follows auto-increments it. Bank 0 is 0 to 152, bank 1 153 to 305 (`mrv_all_bits.h`).
pub const LSC_R_TABLE_ADDR: u32 = LSC_BASE + 0x04;
/// See [`LSC_R_TABLE_ADDR`].
pub const LSC_GR_TABLE_ADDR: u32 = LSC_BASE + 0x08;
/// See [`LSC_R_TABLE_ADDR`].
pub const LSC_B_TABLE_ADDR: u32 = LSC_BASE + 0x0C;
/// See [`LSC_R_TABLE_ADDR`].
pub const LSC_GB_TABLE_ADDR: u32 = LSC_BASE + 0x10;
/// Sample table data per colour: two samples per word (`R_SAMPLE_0` in bits 11..0,
/// `R_SAMPLE_1` in bits 23..12), 2 integer and 10 fractional bits (`400h = 1`).
pub const LSC_R_TABLE_DATA: u32 = LSC_BASE + 0x14;
/// See [`LSC_R_TABLE_DATA`].
pub const LSC_GR_TABLE_DATA: u32 = LSC_BASE + 0x18;
/// See [`LSC_R_TABLE_DATA`].
pub const LSC_B_TABLE_DATA: u32 = LSC_BASE + 0x1C;
/// See [`LSC_R_TABLE_DATA`].
pub const LSC_GB_TABLE_DATA: u32 = LSC_BASE + 0x20;
/// X gradient factors, two per word (`XGRAD_0` bits 11..0, `XGRAD_1` bits 27..16), sectors
/// 0 to 7 in four words at `+0x24`, `+0x28`, `+0x2C`, `+0x30`.
pub const LSC_XGRAD_0: u32 = LSC_BASE + 0x24;
/// Y gradient factors, same layout as [`LSC_XGRAD_0`], at `+0x34` to `+0x40`.
pub const LSC_YGRAD_0: u32 = LSC_BASE + 0x34;
/// X sector sizes, two per word (`X_SECT_SIZE_0` bits 9..0, `_1` bits 25..16), at `+0x44` to
/// `+0x50`.
pub const LSC_XSIZE_0: u32 = LSC_BASE + 0x44;
/// Y sector sizes, same layout as [`LSC_XSIZE_0`], at `+0x54` to `+0x60`.
pub const LSC_YSIZE_0: u32 = LSC_BASE + 0x54;
/// Table set selection (`isp_lsc_table_sel`): 0 selects table set 0.
pub const LSC_TABLE_SEL: u32 = LSC_BASE + 0x64;
/// Bit 0 of [`LSC_CTRL`]: the correction is enabled.
pub const LSC_EN: u32 = 1;
/// Mask of a [`LSC_R_TABLE_ADDR`] register.
pub const LSC_ADDR_MASK: u32 = 0x1FF;
/// Mask of one sample in a table word.
pub const LSC_SAMPLE_MASK: u32 = 0xFFF;
/// Shift of the second sample in a table word.
pub const LSC_SAMPLE_1_SHIFT: u32 = 12;
/// Mask of one sector size or gradient in its word.
pub const LSC_SECT_MASK: u32 = 0x3FF;
/// Mask of one gradient factor (12 bits).
pub const LSC_GRAD_MASK: u32 = 0xFFF;
/// Shift of the second entry in a sector or gradient word.
pub const LSC_PAIR_SHIFT: u32 = 16;

// Histogram (mrv_all_regs.h, `HIST_BASE + offset`).

/// Histogram properties (`isp_hist_prop`): bits 2..0 the mode, bits 9..3 the step size.
pub const HIST_PROP: u32 = HIST_BASE;
/// Window offsets and sizes of the first sub-window (`isp_hist_{h,v}_{offs,size}`).
pub const HIST_H_OFFS: u32 = HIST_BASE + 0x04;
/// See [`HIST_H_OFFS`].
pub const HIST_V_OFFS: u32 = HIST_BASE + 0x08;
/// See [`HIST_H_OFFS`].
pub const HIST_H_SIZE: u32 = HIST_BASE + 0x0C;
/// See [`HIST_H_OFFS`].
pub const HIST_V_SIZE: u32 = HIST_BASE + 0x10;
/// The 16 bin results (`histogram_measurement_result_arr`, `MRV_HIST_BASE + 20 + n*4`), read.
/// Each is a count with 4 fractional bits in 20 bits ([`HIST_BIN_MASK`]). The bits file's
/// `0x028 + n*4` cannot hold: it would overlap the weights at `+0x54`. rkisp1's V10 layout has
/// the same `+0x14` (`RKISP1_CIF_ISP_HIST_BIN_0_V10`).
pub const HIST_BIN_0: u32 = HIST_BASE + 0x14;
/// Sub-window weights, five bits each, packed four to a word (`hist_weight_00to30` ... `_04to34`
/// at `+0x54` to `+0x68`); `hist_weight_44` alone at `+0x6C`.
pub const HIST_WEIGHTS_0: u32 = HIST_BASE + 0x54;
/// See [`HIST_WEIGHTS_0`].
pub const HIST_WEIGHT_44: u32 = HIST_BASE + 0x6C;
/// Histogram mode (`isp_hist_prop` bits 2..0): luma from the ISP output.
pub const HIST_MODE_Y: u32 = 5;
/// Histogram mode field mask.
pub const HIST_MODE_MASK: u32 = 0x7;
/// Step-size field shift in [`HIST_PROP`].
pub const HIST_STEPSIZE_SHIFT: u32 = 3;
/// Histogram bin field mask: 16 integer and 4 fractional bits.
pub const HIST_BIN_MASK: u32 = 0x000F_FFFF;
/// Fractional bits of a histogram bin.
pub const HIST_BIN_FRAC_BITS: u32 = 4;

// Auto exposure (mrv_all_regs.h, `AE_BASE + offset`).

/// Exposure control (`isp_exp_ctrl`): bit 31 selects the luma formula (1: `(R+G+B) x 85/256`,
/// 0: `16 + R/4 + G/2 + 0.1094 B`), bit 1 stops after one frame.
pub const AE_CTRL: u32 = AE_BASE;
/// Block window offsets and the size of one block (`isp_exp_{h,v}_{offset,size}`).
pub const AE_H_OFFSET: u32 = AE_BASE + 0x04;
/// See [`AE_H_OFFSET`].
pub const AE_V_OFFSET: u32 = AE_BASE + 0x08;
/// See [`AE_H_OFFSET`].
pub const AE_H_SIZE: u32 = AE_BASE + 0x0C;
/// See [`AE_H_OFFSET`]. Even only (`0x7FE` mask).
pub const AE_V_SIZE: u32 = AE_BASE + 0x10;
/// The 25 block means (`isp_exp_mean_xy`, read), 8 bits each. Block `(x, y)` is at
/// `AE_MEAN_0 + 4 * (x + 5 * y)`, which is row-major order.
pub const AE_MEAN_0: u32 = AE_BASE + 0x14;
/// Exposure measurement luma formula: bit 31 of [`AE_CTRL`].
pub const AE_CTRL_LUMA_85_256: u32 = 1 << 31;
/// AE means: 8 bits.
pub const AE_MEAN_MASK: u32 = 0xFF;
/// The AE grid's side (`5 x 5` blocks).
pub const AE_GRID: u32 = 5;

// Black level (mrv_all_regs.h, `BLS_BASE + offset`).

/// Black level control (`isp_bls_ctrl`): bit 0 enables the subtraction, bit 1 selects measured
/// (1) or fixed (0) values, bits 3..2 the measuring windows (0: none).
pub const BLS_CTRL: u32 = BLS_BASE;
/// Fixed black levels A to D (`isp_bls_{a,b,c,d}_fixed`), 13 bits signed (subtracted when
/// positive). The brief's Bayer positions are not stated in the headers. The builder takes
/// them in top-left, top-right, bottom-left, bottom-right order of an RGGB mosaic: A = red,
/// B = green (red line), C = green (blue line), D = blue. Unconfirmed.
pub const BLS_A_FIXED: u32 = BLS_BASE + 0x28;
/// See [`BLS_A_FIXED`].
pub const BLS_B_FIXED: u32 = BLS_BASE + 0x2C;
/// See [`BLS_A_FIXED`].
pub const BLS_C_FIXED: u32 = BLS_BASE + 0x30;
/// See [`BLS_A_FIXED`].
pub const BLS_D_FIXED: u32 = BLS_BASE + 0x34;
/// Bit 0 of [`BLS_CTRL`]: subtraction enabled.
pub const BLS_ENABLE: u32 = 1;
/// Fixed level field mask: 13 bits, two's complement.
pub const BLS_FIXED_MASK: u32 = 0x1FFF;

// AWB measurement window (mrv_all_regs.h, `AWB_BASE + offset`).

/// AWB measurement mode (`awb_meas_mode`, the header's first member of the block). Not written
/// by the builder: the RGB mode is set by [`ISP_AWB_PROP`].
pub const AWB_MEAS_MODE: u32 = AWB_BASE;
/// AWB measurement window (`awb_meas_{h,v}_{offs,size}`).
pub const AWB_H_OFFS: u32 = AWB_BASE + 0x04;
/// See [`AWB_H_OFFS`].
pub const AWB_V_OFFS: u32 = AWB_BASE + 0x08;
/// See [`AWB_H_OFFS`].
pub const AWB_H_SIZE: u32 = AWB_BASE + 0x0C;
/// See [`AWB_H_OFFS`].
pub const AWB_V_SIZE: u32 = AWB_BASE + 0x10;

// Sub-window weights.

/// Weight of every sub-window: one, in each five-bit field (`0x01` per byte).
pub const HIST_WEIGHT_ONE_PACKED: u32 = 0x0101_0101;
