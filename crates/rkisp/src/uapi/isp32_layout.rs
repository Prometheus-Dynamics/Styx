//! The ISP32 buffers' `bytemuck` impls, and the C compiler's sizes and offsets for the same
//! headers (checked at compile time: the numbers come from a C program that prints
//! `sizeof` and `offsetof` for `rk-isp32-config.h` from develop-5.10 and develop-6.1, which
//! agree).
//!
//! The ISP32 and ISP32-lite statistics differ: the full ISP32 (RV1106) has the packed 4-byte
//! AE zones ([`isp32_rawae_meas_data`]) and the histogram `rawhist1`; the lite (RK3562) has
//! 6-byte zones ([`isp2x_rawae_meas_data`]) and the big histogram `rawhist3`.

use super::isp32::*;

// SAFETY: the parameter and statistics buffers are `packed` structures of integers, arrays
// and bitfield units of integers, with no padding, so every bit pattern is valid and zero is a
// value.
unsafe impl bytemuck::Zeroable for isp32_isp_params_cfg {}
unsafe impl bytemuck::Pod for isp32_isp_params_cfg {}
unsafe impl bytemuck::Zeroable for rkisp32_isp_stat_buffer {}
unsafe impl bytemuck::Pod for rkisp32_isp_stat_buffer {}
unsafe impl bytemuck::Zeroable for rkisp32_lite_stat_buffer {}
unsafe impl bytemuck::Pod for rkisp32_lite_stat_buffer {}

const _: () = {
    use core::mem::{offset_of, size_of};
    // Parameters.
    assert!(size_of::<isp32_isp_params_cfg>() == 11145);
    assert!(offset_of!(isp32_isp_params_cfg, module_ens) == 8);
    assert!(offset_of!(isp32_isp_params_cfg, module_cfg_update) == 16);
    assert!(offset_of!(isp32_isp_params_cfg, frame_id) == 24);
    assert!(offset_of!(isp32_isp_params_cfg, meas) == 28);
    assert!(offset_of!(isp32_isp_params_cfg, others) == 1966);
    assert!(size_of::<isp32_isp_meas_cfg>() == 1938);
    assert!(offset_of!(isp32_isp_meas_cfg, rawaf) == 0);
    assert!(offset_of!(isp32_isp_meas_cfg, rawawb) == 216);
    assert!(offset_of!(isp32_isp_meas_cfg, rawae0) == 1023);
    assert!(offset_of!(isp32_isp_meas_cfg, rawae3) == 1125);
    assert!(offset_of!(isp32_isp_meas_cfg, rawhist0) == 1171);
    assert!(offset_of!(isp32_isp_meas_cfg, rawhist1) == 1212);
    assert!(offset_of!(isp32_isp_meas_cfg, rawhist3) == 1696);
    assert!(size_of::<isp32_isp_other_cfg>() == 9179);
    assert!(offset_of!(isp32_isp_other_cfg, bls_cfg) == 0);
    assert!(offset_of!(isp32_isp_other_cfg, lsc_cfg) == 230);
    assert!(offset_of!(isp32_isp_other_cfg, awb_gain_cfg) == 2679);
    assert!(offset_of!(isp32_isp_other_cfg, ccm_cfg) == 2829);
    assert!(offset_of!(isp32_isp_other_cfg, gammaout_cfg) == 2908);
    assert!(size_of::<isp32_bls_cfg>() == 44);
    assert!(size_of::<isp32_awb_gain_cfg>() == 32);
    assert!(size_of::<isp32_ccm_cfg>() == 79);
    assert!(size_of::<isp3x_lsc_cfg>() == 2449);
    assert!(size_of::<isp3x_gammaout_cfg>() == 102);
    assert!(size_of::<isp2x_rawaebig_meas_cfg>() == 46);
    assert!(size_of::<isp2x_rawhistbig_cfg>() == 242);
    // Full ISP32 statistics.
    assert!(size_of::<isp32_rawae_meas_data>() == 4);
    assert!(size_of::<isp32_rawaebig_stat0>() == 912);
    assert!(size_of::<isp32_stat>() == 13024);
    assert!(offset_of!(isp32_stat, rawae3_0) == 0);
    assert!(offset_of!(isp32_stat, rawae3_1) == 2920);
    assert!(offset_of!(isp32_stat, bls) == 3064);
    assert!(offset_of!(isp32_stat, rawhist3) == 3072);
    assert!(offset_of!(isp32_stat, rawhist1) == 5120);
    assert!(offset_of!(isp32_stat, rawawb) == 11008);
    assert!(size_of::<rkisp32_isp_stat_buffer>() == 13036);
    assert!(offset_of!(rkisp32_isp_stat_buffer, meas_type) == 13024);
    assert!(offset_of!(rkisp32_isp_stat_buffer, frame_id) == 13028);
    // Lite statistics.
    assert!(size_of::<isp2x_rawae_meas_data>() == 6);
    assert!(size_of::<isp32_lite_rawaebig_stat>() == 1362);
    assert!(offset_of!(isp32_lite_rawaebig_stat, data) == 12);
    assert!(size_of::<isp32_lite_stat>() == 3776);
    assert!(offset_of!(isp32_lite_stat, bls) == 0);
    assert!(offset_of!(isp32_lite_stat, rawae0) == 156);
    assert!(offset_of!(isp32_lite_stat, rawae3) == 306);
    assert!(offset_of!(isp32_lite_stat, rawhist0) == 1668);
    assert!(offset_of!(isp32_lite_stat, rawhist3) == 1924);
    assert!(size_of::<rkisp32_lite_stat_buffer>() == 3788);
    assert!(offset_of!(rkisp32_lite_stat_buffer, meas_type) == 3776);
    assert!(offset_of!(rkisp32_lite_stat_buffer, frame_id) == 3780);
};
