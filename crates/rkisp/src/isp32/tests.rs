use alloc::vec;

use super::{Params, Variant, statistics};
use crate::uapi::isp32::{rkisp32_isp_stat_buffer, rkisp32_lite_stat_buffer};
use crate::uapi::module;
use crate::{CCM_ONE, GAIN_ONE, LSC_ONE};

/// The first zone of a full ISP32 statistics buffer: channelg 0xABC (12 bits), channelb 0x3FF
/// and channelr 0x1F0 (10 bits each), packed as the C compiler lays out
/// `isp32_rawae_meas_data` (checked by the probe that sized the structures).
const ZONE_FULL: u32 = 0x7C3F_FABC;

fn full_stat() -> alloc::vec::Vec<u8> {
    vec![0u8; core::mem::size_of::<rkisp32_isp_stat_buffer>()]
}

fn lite_stat() -> alloc::vec::Vec<u8> {
    vec![0u8; core::mem::size_of::<rkisp32_lite_stat_buffer>()]
}

fn put_u32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u16(buf: &mut [u8], at: usize, v: u16) {
    buf[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

#[test]
fn parameters_carry_the_modules_and_their_values() {
    let p = Params::new(Variant::Isp32)
        .frame(7)
        .measure(1280, 800)
        .black_level([256, 256, 256, 256])
        .white_balance([1.5, 1.0, 2.0])
        .colour_matrix(
            [[1.5, -0.25, -0.25], [-0.25, 1.5, -0.25], [0.0, -0.5, 1.5]],
            [0.0; 3],
        )
        .gamma(|x| x.powf(1.0 / 2.2));
    assert_eq!(p.as_bytes().len(), 11145);
    assert_eq!(p.variant(), Variant::Isp32);
    let c = p.config();
    let want = module::RAWAE3
        | module::RAWHIST1
        | module::BLS
        | module::AWB_GAIN
        | module::CCM
        | module::GOC;
    assert_eq!({ c.module_ens }, want);
    assert_eq!({ c.module_en_update }, want);
    assert_eq!({ c.module_cfg_update }, want);
    assert_eq!({ c.frame_id }, 7);
    let bls = c.others.bls_cfg;
    assert_eq!({ bls.fixed_val.r }, 256);
    let w = c.others.awb_gain_cfg;
    assert_eq!(
        ({ w.gain0_red }, { w.gain0_green_r }, { w.gain0_blue }),
        (384, 256, 512)
    );
    assert_eq!(({ w.gain2_red }, { w.gain2_green_b }), (384, 256));
    let m = c.others.ccm_cfg;
    assert_eq!(
        ({ m.coeff0_r }, { m.coeff1_r }, { m.coeff1_b }),
        (192, -32, -64)
    );
    assert_eq!(({ m.bound_bit }, { m.right_bit }), (0, 0));
    let g = c.others.gammaout_cfg;
    let y = { g.gamma_y };
    assert_eq!((y[0], y[48]), (0, 4095));
    assert!(y[24] > 2048, "gamma lifts the middle: {}", y[24]);
    let ae = c.meas.rawae3;
    let win = ae.win;
    assert_eq!(({ win.h_size }, { win.v_size }, ae.wnd_num), (1280, 800, 2));
    let h = c.meas.rawhist1;
    let hw = h.win;
    assert_eq!(({ hw.h_size }, { hw.v_size }, h.mode), (1280, 800, 5));
    assert_eq!((h.rcc, h.gcc, h.bcc), (77, 150, 29));
}

#[test]
fn the_lite_measures_its_big_histogram_block() {
    let p = Params::new(Variant::Isp32Lite).measure(1280, 800);
    let c = p.config();
    assert_eq!({ c.module_ens }, module::RAWAE3 | module::RAWHIST3);
    assert_eq!(
        { c.module_ens } & module::RAWHIST1,
        0,
        "the lite masks rawhist1 out"
    );
    let h = c.meas.rawhist3;
    let hw = h.win;
    assert_eq!(({ hw.h_size }, { hw.v_size }, h.wnd_num), (1280, 800, 2));
    assert_eq!({ c.meas.rawhist1.wnd_num }, 0, "rawhist1 is not configured");
}

#[test]
fn a_zero_white_balance_gain_is_written_as_the_smallest_step() {
    // The driver refuses the whole block when any gain is zero (`isp_awbgain_config`).
    let p = Params::new(Variant::Isp32).white_balance([0.0, 0.0, 1.0e9]);
    let w = p.config().others.awb_gain_cfg;
    assert_eq!(({ w.gain0_red }, { w.gain0_green_r }), (1, 1));
    assert_eq!({ w.gain0_blue }, 65535, "clamped at the 8.8 range's top");
    assert!({ p.config().module_ens } & module::AWB_GAIN != 0);
}

#[test]
fn lens_shading_is_resampled_to_the_vertex_grid() {
    // A 2x2 table: corners take the corner cells' gains, the middle their mean.
    let r = [1.0, 1.5, 2.0, 2.5];
    let flat = [1.0; 4];
    let p = Params::new(Variant::Isp32).lens_shading(1280, 800, (2, 2), [&r, &flat, &flat]);
    let c = p.config();
    assert_eq!({ c.module_ens }, module::LSC);
    let lsc = c.others.lsc_cfg;
    let t = { lsc.r_data_tbl };
    let one = LSC_ONE as u16;
    assert_eq!(
        (t[0], t[16], t[16 * 17], t[16 * 17 + 16]),
        (one, 1536, 2 * one, 2560)
    );
    assert_eq!(t[8 * 17 + 8], (1.75 * LSC_ONE) as u16);
    assert_eq!({ lsc.gr_data_tbl }[100], one);
    let xs = { lsc.x_size_tbl };
    assert_eq!(xs.iter().map(|&v| u32::from(v)).sum::<u32>(), 640);
    assert_eq!(
        { lsc.y_size_tbl }
            .iter()
            .map(|&v| u32::from(v))
            .sum::<u32>(),
        400
    );
    assert_eq!({ lsc.x_grad_tbl }[0], 819, "32768 / 40");
    let p = Params::new(Variant::Isp32).lens_shading(1280, 800, (1, 1), [&[5.0], &[1.0], &[1.0]]);
    assert_eq!({ p.config().others.lsc_cfg.r_data_tbl }[0], 4095);
    let p = Params::new(Variant::Isp32).lens_shading(1280, 800, (3, 3), [&r, &flat, &flat]);
    assert_eq!({ p.config().module_ens }, 0);
}

#[test]
fn no_curve_is_the_srgb_curve_and_the_constants_are_the_isp30_scales() {
    let p = Params::new(Variant::Isp32).gamma(|x| x);
    let g = { p.config().others.gammaout_cfg.gamma_y };
    assert_eq!((g[0], g[48]), (0, 4095));
    assert_eq!(GAIN_ONE, 256.0);
    assert_eq!(CCM_ONE, 128.0);
}

#[test]
fn the_full_statistics_decode_packed_zones_and_rawhist1() {
    let mut buf = full_stat();
    // Zone 1 at the rawae3_0 block (offset 0), 4 bytes each.
    put_u32(&mut buf, 4, ZONE_FULL);
    // rawhist1 at offset 5120 (`isp32_stat`), bin 10; rawhist3 (offset 3072) holds other data.
    put_u32(&mut buf, 5120 + 10 * 4, 1000);
    put_u32(&mut buf, 3072 + 10 * 4, 7);
    let mut out = styx_algo::Statistics::default();
    statistics(Variant::Isp32, &buf, 100, &mut out).unwrap();
    assert_eq!((out.colour.width, out.colour.height), (15, 15));
    assert_eq!(out.colour.zones.len(), 225);
    let z1 = out.colour.zones[1];
    assert!((z1.r / 100.0 - 0x1F0 as f64 / 1023.0).abs() < 1e-9);
    assert!((z1.g / 100.0 - 0xABC as f64 / 4095.0).abs() < 1e-9);
    assert!((z1.b / 100.0 - 0x3FF as f64 / 1023.0).abs() < 1e-9);
    assert_eq!(z1.counted, 100);
    let z0 = out.colour.zones[0];
    assert_eq!((z0.r, z0.g, z0.b), (0.0, 0.0, 0.0));
    assert_eq!(out.histogram.bins()[10], 1000);
    assert!(out.before_wb && out.before_lsc);
    assert!(statistics(Variant::Isp32, &[0u8; 100], 100, &mut out).is_none());
}

#[test]
fn the_lite_statistics_decode_16_bit_zones_and_rawhist3() {
    let mut buf = lite_stat();
    // rawae3 at offset 306; its zones start 12 bytes in (sumr, sumg, sumb), 6 bytes each:
    // channelr_xy, channelb_xy, channelg_xy as 16-bit fields.
    let zone1 = 306 + 12 + 6;
    put_u16(&mut buf, zone1, 512);
    put_u16(&mut buf, zone1 + 2, 256);
    put_u16(&mut buf, zone1 + 4, 4095);
    // rawhist3 at offset 1924; rawhist1 does not exist on the lite.
    put_u32(&mut buf, 1924 + 10 * 4, 1000);
    let mut out = styx_algo::Statistics::default();
    statistics(Variant::Isp32Lite, &buf, 100, &mut out).unwrap();
    assert_eq!(out.colour.zones.len(), 225);
    let z1 = out.colour.zones[1];
    assert!((z1.r / 100.0 - 512.0 / 1023.0).abs() < 1e-9);
    assert!((z1.g / 100.0 - 1.0).abs() < 1e-9);
    assert!((z1.b / 100.0 - 256.0 / 1023.0).abs() < 1e-9);
    assert_eq!(out.histogram.bins()[10], 1000);
    assert!(statistics(Variant::Isp32Lite, &[0u8; 100], 100, &mut out).is_none());
}

#[test]
fn isp32_reads_rawhist1_not_the_lite_block() {
    // rawhist3 holds data, rawhist1 does not: the full ISP32 histogram is rawhist1 only.
    let mut buf = full_stat();
    put_u32(&mut buf, 3072 + 10 * 4, 7);
    let mut out = styx_algo::Statistics::default();
    statistics(Variant::Isp32, &buf, 100, &mut out).unwrap();
    assert_eq!(
        out.histogram.bins()[10],
        0,
        "ISP32 reads rawhist1, not rawhist3"
    );
}
