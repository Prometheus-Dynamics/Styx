use crate::uapi::{module, rkisp3x_isp_stat_buffer};
use crate::*;

#[test]
fn parameters_carry_the_modules_and_their_values() {
    let p = Params::new()
        .frame(7)
        .measure(1280, 800)
        .black_level([256, 256, 256, 256])
        .white_balance([1.5, 1.0, 2.0])
        .colour_matrix(
            [[1.5, -0.25, -0.25], [-0.25, 1.5, -0.25], [0.0, -0.5, 1.5]],
            [0.0; 3],
        )
        .gamma(|x| x.powf(1.0 / 2.2));
    assert_eq!(p.as_bytes().len(), 10921);
    let c = p.config();
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
    statistics(bytemuck::bytes_of(&stat), 100, &mut out).unwrap();
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
    assert!(statistics(&[0u8; 100], 100, &mut out).is_none());
}

#[test]
fn lens_shading_is_resampled_to_the_vertex_grid() {
    // A 2x2 table: corners take the corner cells' gains, the middle their mean.
    let r = [1.0, 1.5, 2.0, 2.5];
    let flat = [1.0; 4];
    let p = Params::new().lens_shading(1280, 800, (2, 2), [&r, &flat, &flat]);
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
    let p = Params::new().lens_shading(1280, 800, (1, 1), [&[5.0], &[1.0], &[1.0]]);
    assert_eq!({ p.config().others.lsc_cfg.r_data_tbl }[0], 4095);
    // A table of the wrong size changes nothing.
    let p = Params::new().lens_shading(1280, 800, (3, 3), [&r, &flat, &flat]);
    assert_eq!({ p.config().module_ens }, 0);
}
