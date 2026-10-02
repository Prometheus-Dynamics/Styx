//! 3A statistics on synthetic mosaics with known zone contents, and parallel bands giving the
//! same pixels and statistics as one thread.

mod common;

use common::*;
use styx_softisp::*;

const W: usize = 64;
const H: usize = 48;

fn stats_params(config: StatsConfig) -> IspParams {
    IspParams {
        stats: Some(config),
        ..Default::default()
    }
}

#[test]
fn zone_sums_counts_luma_and_histogram() {
    // Left half one colour, right half another; bottom-right quarter clipped (1023).
    let (a, b) = ([400u16, 600, 200], [100u16, 300, 900]);
    let m = mosaic(W, H, CfaPattern::Bggr, |x, y| {
        if x >= W / 2 && y >= H / 2 {
            [1023; 3]
        } else if x < W / 2 {
            a
        } else {
            b
        }
    });
    let (bytes, stride) = pack(&m, W, H, RawPacking::Csi2Raw10);
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let config = StatsConfig {
        zones_x: 4,
        zones_y: 4,
        histogram_bins: 64,
        ..Default::default()
    };
    let (_, stats) = rgb(format, &stats_params(config), &bytes, stride, Scale::Full);
    let stats = stats.unwrap();
    assert_eq!(
        (stats.zones_x, stats.zones_y, stats.zones.len()),
        (4, 4, 16)
    );
    assert_eq!(stats.samples, (W * H / 4) as u32);
    assert_eq!(stats.gains, [1.0; 3]);
    // Each zone: 8 x 6 quads.
    let quads = 8 * 6;
    let work = |v: u16| v as u64 * 4;
    for zy in 0..4 {
        for zx in 0..4 {
            let z = stats.zone(zx, zy);
            let (colour, clipped) = match (zx < 2, zy < 2) {
                (true, _) => (a, false),
                (false, true) => (b, false),
                (false, false) => ([1023; 3], true),
            };
            if clipped {
                assert_eq!((z.count, z.r_sum), (0, 0), "zone {zx},{zy}");
                assert!((z.luma - 1.0).abs() < 0.01);
                assert_eq!(z.mean_rgb(), None);
            } else {
                assert_eq!(z.count, quads);
                assert_eq!(
                    [z.r_sum, z.g_sum, z.b_sum],
                    colour.map(|c| work(c) * quads as u64),
                    "zone {zx},{zy}"
                );
                let luma =
                    (work(colour[0]) + 2 * work(colour[1]) + work(colour[2])) as f32 / 4.0 / 4095.0;
                assert!(
                    (z.luma - luma).abs() < 1e-3,
                    "zone {zx},{zy}: {} vs {luma}",
                    z.luma
                );
                let mean = z.mean_rgb().unwrap();
                assert!((mean[0] - work(colour[0]) as f32 / 4095.0).abs() < 1e-4);
            }
        }
    }
    // Histogram: three populations.
    let bin =
        |c: [u16; 3]| ((work(c[0]) + 2 * work(c[1]) + work(c[2]) + 2) / 4 * 64 / 4096) as usize;
    let per_region = (W / 2 * H / 2 / 4) as u32;
    assert_eq!(stats.histogram.len(), 64);
    assert_eq!(stats.histogram[bin(a)], 2 * per_region);
    assert_eq!(stats.histogram[bin(b)], per_region);
    assert_eq!(stats.histogram[63], per_region);
    assert_eq!(stats.histogram.iter().sum::<u32>(), stats.samples);
}

#[test]
fn stats_see_the_gains_and_grey_world_undoes_them() {
    let m = mosaic(W, H, CfaPattern::Rggb, |_, _| [300; 3]);
    let (bytes, stride) = pack(&m, W, H, RawPacking::Csi2Raw10);
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
    let params = IspParams {
        white_balance: Some(WhiteBalance {
            r: 2.0,
            g: 1.0,
            b: 0.5,
        }),
        ..stats_params(StatsConfig::default())
    };
    let (_, stats) = rgb(format, &params, &bytes, stride, Scale::Full);
    let stats = stats.unwrap();
    assert_eq!(stats.gains, [2.0, 1.0, 0.5]);
    let g = stats.grey_world_gains().unwrap();
    assert!(
        (g[0] - 0.5).abs() < 0.01 && (g[2] - 2.0).abs() < 0.01,
        "{g:?}"
    );
}

#[test]
fn row_step_and_all_outputs_and_scales_gather_the_same_stats() {
    let m = mosaic(W, H, CfaPattern::Gbrg, |x, y| {
        [
            (x * 13 % 1024) as u16,
            (y * 17 % 1024) as u16,
            ((x + y) * 7 % 1024) as u16,
        ]
    });
    let (bytes, stride) = pack(&m, W, H, RawPacking::Csi2Raw10);
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Gbrg, RawPacking::Csi2Raw10);
    let params = stats_params(StatsConfig {
        zones_x: 8,
        zones_y: 6,
        ..Default::default()
    });
    let mut all = Vec::new();
    for scale in [Scale::Full, Scale::Half] {
        let (ow, oh) = if scale == Scale::Full {
            (W, H)
        } else {
            (W / 2, H / 2)
        };
        let (mut a, mut b) = (vec![0u8; ow * oh * 3], vec![0u8; ow * oh]);
        all.push(
            process(
                format,
                &params,
                &bytes,
                stride,
                scale,
                OutputBuffers::Rgb24 {
                    data: &mut a,
                    stride: ow * 3,
                },
            )
            .unwrap(),
        );
        all.push(
            process(
                format,
                &params,
                &bytes,
                stride,
                scale,
                OutputBuffers::Luma {
                    data: &mut b,
                    stride: ow,
                },
            )
            .unwrap(),
        );
        let mut uv = vec![0u8; ow * oh / 2];
        all.push(
            process(
                format,
                &params,
                &bytes,
                stride,
                scale,
                OutputBuffers::Nv12 {
                    y: &mut b,
                    y_stride: ow,
                    uv: &mut uv,
                    uv_stride: ow,
                },
            )
            .unwrap(),
        );
    }
    assert!(all.windows(2).all(|w| w[0] == w[1]));
    let stepped = stats_params(StatsConfig {
        zones_x: 8,
        zones_y: 6,
        row_step: 2,
        ..Default::default()
    });
    let (_, s) = rgb(format, &stepped, &bytes, stride, Scale::Full);
    assert_eq!(s.unwrap().samples, all[0].as_ref().unwrap().samples / 2);
}

#[test]
fn parallel_bands_match_one_thread() {
    let (w, h) = (256usize, 150usize);
    let m = mosaic(w, h, CfaPattern::Bggr, |x, y| {
        [
            (x * 5 % 1024) as u16,
            (y * 11 % 1024) as u16,
            ((x ^ y) % 1024) as u16,
        ]
    });
    let (bytes, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
    let format = RawFormat::new(w as u32, h as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    for demosaic in [Demosaic::Bilinear, Demosaic::Mhc] {
        let params = IspParams {
            demosaic,
            stats: Some(StatsConfig::default()),
            ..Default::default()
        };
        let mut one = SoftIsp::new(format, params.clone()).unwrap();
        for threads in [0, 2, 3, 4, 7] {
            let mut many = SoftIsp::new(format, params.clone())
                .unwrap()
                .with_threads(threads);
            for scale in [Scale::Full, Scale::Half] {
                let (ow, oh) = if scale == Scale::Full {
                    (w, h)
                } else {
                    (w / 2, h / 2)
                };
                let run = |isp: &mut SoftIsp| {
                    let (mut rgb, mut y, mut uv) = (
                        vec![0u8; ow * oh * 3],
                        vec![0u8; ow * oh],
                        vec![0u8; ow * oh.div_ceil(2)],
                    );
                    let s1 = isp
                        .process(
                            &bytes,
                            stride,
                            scale,
                            OutputBuffers::Rgb24 {
                                data: &mut rgb,
                                stride: ow * 3,
                            },
                        )
                        .unwrap();
                    let s2 = if oh % 2 == 0 {
                        isp.process(
                            &bytes,
                            stride,
                            scale,
                            OutputBuffers::Nv12 {
                                y: &mut y,
                                y_stride: ow,
                                uv: &mut uv,
                                uv_stride: ow,
                            },
                        )
                        .unwrap()
                    } else {
                        isp.process(
                            &bytes,
                            stride,
                            scale,
                            OutputBuffers::Luma {
                                data: &mut y,
                                stride: ow,
                            },
                        )
                        .unwrap()
                    };
                    (rgb, y, uv, s1, s2)
                };
                assert!(
                    run(&mut one) == run(&mut many),
                    "{demosaic:?} {threads} threads {scale:?}"
                );
            }
        }
    }
}
