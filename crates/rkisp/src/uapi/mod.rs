//! The Rockchip ISP30 parameter and statistics buffers as the vendor kernel's `rkisp` driver
//! takes and fills them (`rk-isp3-config.h`), with the module bits it reads from
//! `module_ens` / `module_en_update` / `module_cfg_update`.
//!
//! The layouts are bindgen's ([`isp3x`]); their sizes and the offsets of the blocks Styx uses
//! are checked here against the C compiler's for the same headers.

pub mod isp3x;

pub use isp3x::*;

/// A module's bit in the parameter buffer's `module_*` masks (`ISP2X_ID_*` of
/// `rk-isp2-config.h`; bindgen does not expand the headers' `_BITULL` macros).
pub mod module {
    pub const DPCC: u64 = 1 << 0;
    pub const BLS: u64 = 1 << 1;
    pub const SDG: u64 = 1 << 2;
    pub const LSC: u64 = 1 << 4;
    pub const AWB_GAIN: u64 = 1 << 5;
    pub const BDM: u64 = 1 << 7;
    pub const CCM: u64 = 1 << 8;
    pub const GOC: u64 = 1 << 9;
    pub const CPROC: u64 = 1 << 10;
    pub const RAWAF: u64 = 1 << 17;
    pub const RAWAE0: u64 = 1 << 18;
    pub const RAWAE1: u64 = 1 << 19;
    pub const RAWAE2: u64 = 1 << 20;
    pub const RAWAE3: u64 = 1 << 21;
    pub const RAWAWB: u64 = 1 << 22;
    pub const RAWHIST0: u64 = 1 << 23;
    pub const RAWHIST1: u64 = 1 << 24;
    pub const RAWHIST2: u64 = 1 << 25;
    pub const RAWHIST3: u64 = 1 << 26;
    pub const GIC: u64 = 1 << 30;
    pub const DHAZ: u64 = 1 << 31;
    pub const GAIN: u64 = 1 << 34;
    pub const DEBAYER: u64 = 1 << 35;
    pub const BAYNR: u64 = 1 << 36;
    pub const BAY3D: u64 = 1 << 37;
    pub const YNR: u64 = 1 << 38;
    pub const CNR: u64 = 1 << 39;
    pub const SHARP: u64 = 1 << 40;
    pub const DRC: u64 = 1 << 41;
    pub const CSM: u64 = 1 << 43;
    pub const CGC: u64 = 1 << 44;
}

// SAFETY: both are `#[repr(C, packed)]` structures of integers and arrays and structures of
// integers, with no padding (packed), so every bit pattern is valid and zero is a value.
unsafe impl bytemuck::Zeroable for isp3x_isp_params_cfg {}
unsafe impl bytemuck::Pod for isp3x_isp_params_cfg {}
unsafe impl bytemuck::Zeroable for rkisp3x_isp_stat_buffer {}
unsafe impl bytemuck::Pod for rkisp3x_isp_stat_buffer {}

/// The C compiler's sizes and offsets for the same headers (clang, aarch64 and x86-64 alike:
/// the structures are packed).
const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<isp3x_isp_params_cfg>() == 10921);
    assert!(size_of::<rkisp3x_isp_stat_buffer>() == 16172);
    assert!(size_of::<isp3x_isp_meas_cfg>() == 2012);
    assert!(size_of::<isp3x_isp_other_cfg>() == 8785);
    assert!(size_of::<isp3x_stat>() == 16160);
    assert!(offset_of!(isp3x_isp_params_cfg, meas) == 28);
    assert!(offset_of!(isp3x_isp_params_cfg, others) == 2040);
    assert!(offset_of!(isp3x_isp_params_cfg, exposure) == 10825);
    assert!(offset_of!(isp3x_isp_other_cfg, bls_cfg) == 0);
    assert!(offset_of!(isp3x_isp_other_cfg, awb_gain_cfg) == 2671);
    assert!(offset_of!(isp3x_isp_other_cfg, ccm_cfg) == 2780);
    assert!(offset_of!(isp3x_isp_other_cfg, gammaout_cfg) == 2846);
    assert!(offset_of!(isp3x_stat, rawae0) == 4194);
    assert!(offset_of!(isp3x_stat, rawhist1) == 6392);
    assert!(offset_of!(isp3x_stat, rawawb) == 12056);
    assert!(offset_of!(isp3x_stat, bls) == 16152);
    assert!(offset_of!(isp3x_isp_meas_cfg, rawawb) == 191);
    assert!(offset_of!(isp3x_isp_meas_cfg, rawae0) == 1097);
    assert!(offset_of!(isp3x_isp_meas_cfg, rawhist1) == 1286);
};
