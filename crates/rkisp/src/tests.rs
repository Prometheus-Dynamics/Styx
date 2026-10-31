use crate::uapi::{isp21, module, rkisp3x_isp_stat_buffer};
use crate::*;

#[test]
fn parameters_carry_the_modules_and_their_values() {
    let p = Params::new(Generation::Isp30)
        .frame(7)
        .measure(1280, 800)
        .black_level([256, 256, 256, 256])
        .white_balance([1.5, 1.0, 2.0])
        .colour_matrix(
            [[1.5, -0.25, -0.25], [-0.25, 1.5, -0.25], [0.0, -0.5, 1.5]],
            [0.0; 3],
        )
        .gamma(|x| x.powf(1.0 / 2.2));
    assert_eq!(p.generation(), Generation::Isp30);
    assert_eq!(p.as_bytes().len(), 10921);
    let c = p.isp30().unwrap();
    let ens = c.module_ens;
    let want = module::RAWAE3
        | module::RAWHIST1
        | module::BLS
        | module::AWB_GAIN
        | module::CCM
        | module::GOC;
    assert_eq!(ens, want);
    assert_eq!({ c.module_cfg_update }, want);
    assert_eq!({ c.frame_id }, 7);
    let w = c.others.awb_gain_cfg;
    assert_eq!(
        ({ w.gain0_red }, { w.gain0_green_r }, { w.gain0_blue }),
        (384, 256, 512)
    );
    let m = c.others.ccm_cfg;
    assert_eq!(
        ({ m.coeff0_r }, { m.coeff1_r }, { m.coeff1_b }),
        (192, -32, -64)
    );
    let g = c.others.gammaout_cfg;
    let y = g.gamma_y;
    assert_eq!((y[0], y[48]), (0, 4095));
    assert!(y[24] > 2048, "gamma lifts the middle: {}", y[24]);
    let ae = c.meas.rawae3;
    let win = ae.win;
    assert_eq!(({ win.h_size }, { win.v_size }, ae.wnd_num), (1280, 800, 2));
    assert!(p.isp21().is_none());
}

#[test]
fn statistics_become_colour_zones_and_a_histogram() {
    let mut stat: rkisp3x_isp_stat_buffer = bytemuck::Zeroable::zeroed();
    let mut data = stat.params.rawae3.data;
    for (i, z) in data.iter_mut().enumerate() {
        z.channelr_xy = 512;
        z.channelg_xy = if i == 0 { 4095 } else { 1024 };
        z.channelb_xy = 256;
    }
    stat.params.rawae3.data = data;
    let mut bins = [0u32; 256];
    bins[10] = 1000;
    bins[255] = 5;
    stat.params.rawhist1.hist_bin = bins;
    let mut out = styx_algo::Statistics::default();
    statistics(Generation::Isp30, bytemuck::bytes_of(&stat), 100, &mut out).unwrap();
    assert_eq!((out.colour.width, out.colour.height), (15, 15));
    assert_eq!(out.colour.zones.len(), 225);
    let z0 = out.colour.zones[0];
    assert!((z0.g / 100.0 - 1.0).abs() < 1e-9);
    let z1 = out.colour.zones[1];
    assert!((z1.r / 100.0 - 512.0 / 1023.0).abs() < 1e-9);
    assert!((z1.g / 100.0 - 1024.0 / 4095.0).abs() < 1e-9);
    assert_eq!(z1.counted, 100);
    assert_eq!(out.histogram.bins()[10], 1000);
    assert!(out.before_wb && out.before_lsc);
    // Too short a buffer is refused.
    assert!(statistics(Generation::Isp30, &[0u8; 100], 100, &mut out).is_none());
}

#[test]
fn lens_shading_is_resampled_to_the_vertex_grid() {
    // A 2x2 table: corners take the corner cells' gains, the middle their mean.
    let r = [1.0, 1.5, 2.0, 2.5];
    let flat = [1.0; 4];
    let p = Params::new(Generation::Isp30).lens_shading(1280, 800, (2, 2), [&r, &flat, &flat]);
    let c = p.isp30().unwrap();
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
    assert_eq!({ lsc.sector_16x16 }, 1);
    // 16 sectors over the 640x400 Bayer quads.
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
    // Gains beyond the 12-bit sample's 4.0 are clamped.
    let p =
        Params::new(Generation::Isp30).lens_shading(1280, 800, (1, 1), [&[5.0], &[1.0], &[1.0]]);
    assert_eq!({ p.isp30().unwrap().others.lsc_cfg.r_data_tbl }[0], 4095);
    // A table of the wrong size changes nothing.
    let p = Params::new(Generation::Isp30).lens_shading(1280, 800, (3, 3), [&r, &flat, &flat]);
    assert_eq!({ p.isp30().unwrap().module_ens }, 0);
}

#[test]
fn zero_gains_are_written_as_the_driver_minimum() {
    // The driver refuses a buffer with a zero gain, so a zero gain goes out as 1/256.
    for generation in [Generation::Isp21, Generation::Isp30] {
        let p = Params::new(generation).white_balance([0.0, 1.0, 0.0]);
        let (red, blue) = match generation {
            Generation::Isp21 => {
                let w = p.isp21().unwrap().others.awb_gain_cfg;
                ({ w.gain0_red }, { w.gain0_blue })
            }
            Generation::Isp30 => {
                let w = p.isp30().unwrap().others.awb_gain_cfg;
                ({ w.gain0_red }, { w.gain0_blue })
            }
        };
        assert_eq!((red, blue), (1, 1), "{generation:?}");
    }
}

#[test]
fn isp21_parameters_carry_the_modules_and_their_values() {
    let p = Params::new(Generation::Isp21)
        .frame(9)
        .measure(1280, 800)
        .black_level([256, 256, 256, 256])
        .white_balance([1.5, 1.0, 2.0])
        .colour_matrix(
            [[1.5, -0.25, -0.25], [-0.25, 1.5, -0.25], [0.0, -0.5, 1.5]],
            [0.0; 3],
        )
        .gamma(|x| x.powf(1.0 / 2.2));
    assert_eq!(p.generation(), Generation::Isp21);
    assert_eq!(p.as_bytes().len(), 10475);
    let c = p.isp21().unwrap();
    let want = module::RAWAE3
        | module::RAWHIST1
        | module::BLS
        | module::AWB_GAIN
        | module::CCM
        | module::GOC;
    assert_eq!({ c.module_ens }, want);
    assert_eq!({ c.module_cfg_update }, want);
    assert_eq!({ c.frame_id }, 9);
    let bls = c.others.bls_cfg;
    assert_eq!({ bls.fixed_val.r }, 256);
    let w = c.others.awb_gain_cfg;
    assert_eq!(
        ({ w.gain0_red }, { w.gain0_green_r }, { w.gain0_blue }),
        (384, 256, 512)
    );
    assert_eq!({ w.gain2_green_b }, 256);
    let m = c.others.ccm_cfg;
    assert_eq!(({ m.coeff0_r }, { m.coeff1_b }), (192, -64));
    // The ISP21 gamma has 45 samples, the last one at the full scale.
    let g = c.others.gammaout_cfg;
    let y = g.gamma_y;
    assert_eq!(y.len(), ISP21_GAMMA_SAMPLES);
    assert_eq!((g.equ_segm, y[0], y[44]), (1, 0, 4095));
    assert!(y[22] > 2048, "gamma lifts the middle: {}", y[22]);
    let win = c.meas.rawae3.win;
    assert_eq!(
        ({ win.h_size }, { win.v_size }, c.meas.rawae3.wnd_num),
        (1280, 800, 2)
    );
    assert!(p.isp30().is_none());
}

#[test]
fn isp21_lens_shading_is_eight_by_eight() {
    let r = [1.0, 1.5, 2.0, 2.5];
    let flat = [1.0; 4];
    let p = Params::new(Generation::Isp21).lens_shading(1280, 800, (2, 2), [&r, &flat, &flat]);
    let c = p.isp21().unwrap();
    assert_eq!({ c.module_ens }, module::LSC);
    let lsc = c.others.lsc_cfg;
    // 8 sectors over the 640x400 Bayer quads, 80 and 50 quads wide.
    assert_eq!({ lsc.x_size_tbl }, [80; 8]);
    assert_eq!({ lsc.y_size_tbl }, [50; 8]);
    assert_eq!({ lsc.x_grad_tbl }[0], 410, "(32768 + 40) / 80");
    // The 9x9 vertices sit in the top-left of the 17-wide table; the rest stays at unity.
    let t = { lsc.r_data_tbl };
    let one = LSC_ONE as u16;
    assert_eq!(
        (t[0], t[8], t[8 * 17], t[8 * 17 + 8]),
        (one, 1536, 2 * one, 2560)
    );
    assert_eq!(t[4 * 17 + 4], (1.75 * LSC_ONE) as u16);
    assert_eq!(t[9], one);
    assert_eq!(t[9 * 17 + 3], one);
    assert_eq!({ lsc.gr_data_tbl }[100], one);
    assert_eq!({ lsc.b_data_tbl }[0], one);
    // A table of the wrong size changes nothing.
    let p = Params::new(Generation::Isp21).lens_shading(1280, 800, (3, 3), [&r, &flat, &flat]);
    assert_eq!({ p.isp21().unwrap().module_ens }, 0);
}

#[test]
fn isp21_statistics_become_colour_zones_and_a_histogram() {
    let mut stat: isp21::rkisp_isp21_stat_buffer = bytemuck::Zeroable::zeroed();
    let mut data = stat.params.rawae3.data;
    for (i, z) in data.iter_mut().enumerate() {
        z.channelr_xy = 512;
        z.channelg_xy = if i == 0 { 4095 } else { 1024 };
        z.channelb_xy = 256;
    }
    stat.params.rawae3.data = data;
    let mut bins = [0u32; 256];
    bins[10] = 1000;
    bins[255] = 5;
    stat.params.rawhist1.hist_bin = bins;
    let bytes = bytemuck::bytes_of(&stat);
    assert_eq!(bytes.len(), 13700);
    let mut out = styx_algo::Statistics::default();
    statistics(Generation::Isp21, bytes, 100, &mut out).unwrap();
    assert_eq!((out.colour.width, out.colour.height), (15, 15));
    assert_eq!(out.colour.zones.len(), 225);
    assert!((out.colour.zones[0].g / 100.0 - 1.0).abs() < 1e-9);
    let z1 = out.colour.zones[1];
    assert!((z1.r / 100.0 - 512.0 / 1023.0).abs() < 1e-9);
    assert!((z1.b / 100.0 - 256.0 / 1023.0).abs() < 1e-9);
    assert_eq!(z1.counted, 100);
    assert_eq!(out.histogram.bins()[10], 1000);
    assert_eq!(out.histogram.bins()[255], 5);
    assert!(out.before_wb && out.before_lsc);
    // A short buffer is refused.
    assert!(statistics(Generation::Isp21, &bytes[..13699], 100, &mut out).is_none());
}
