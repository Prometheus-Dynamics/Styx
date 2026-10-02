//! Software ISP on a 1280x800 RAW10 frame (the OV9782's `pBAA`, stride 1600): each stage over
//! a whole frame's rows, and whole pipelines end to end.
//!
//! `cargo bench -p styx-softisp`.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use styx_softisp::simd::{self, RowKind, YuvCoeffs};
use styx_softisp::*;

/// Keeps a kernel's result (the backend it used) observable.
fn sink(backend: simd::SimdBackend) {
    black_box(backend);
}

const W: usize = 1280;
const H: usize = 800;
const STRIDE: usize = 1600;

/// A textured BGGR RAW10 frame: gradients plus pseudo-random noise.
fn frame() -> Vec<u8> {
    let mut raw = vec![0u8; STRIDE * H];
    let mut seed = 0x1234_5678u32;
    for y in 0..H {
        for g in 0..W / 4 {
            let mut low = 0u8;
            for i in 0..4 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let x = g * 4 + i;
                let v = ((x + y) as u32 % 700 + 64 + (seed & 63)).min(1023) as u16;
                raw[y * STRIDE + g * 5 + i] = (v >> 2) as u8;
                low |= ((v & 3) as u8) << (2 * i);
            }
            raw[y * STRIDE + g * 5 + 4] = low;
        }
    }
    raw
}

fn tuned(demosaic: Demosaic) -> IspParams {
    IspParams {
        black_level: Some(BlackLevel::uniform(64)),
        white_balance: Some(WhiteBalance {
            r: 1.9,
            g: 1.0,
            b: 1.6,
        }),
        demosaic,
        ccm: Some(ColorMatrix {
            m: [[1.6, -0.4, -0.2], [-0.3, 1.5, -0.2], [-0.1, -0.5, 1.6]],
        }),
        tone: Some(ToneCurve::Srgb),
        ..Default::default()
    }
}

fn configure() -> Criterion {
    Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(1))
}

/// Each kernel over a frame's worth of rows (from rows in cache, as in the pipeline).
fn stages(c: &mut Criterion) {
    let raw = frame();
    let row16: Vec<u16> = (0..W + 4).map(|i| (i * 37 % 4096) as u16).collect();
    let gains = vec![6000u16; W];
    let (mut a, mut b, mut d) = (vec![0u16; W + 4], vec![0u16; W], vec![0u16; W]);
    let mut e = vec![0u16; W];
    let (mut p8, mut q8, mut s8) = (vec![0u8; W], vec![0u8; W], vec![0u8; W]);
    let mut rgb = vec![0u8; W * 3];
    let lut = simd::ToneLut::from_curve(|x| x.sqrt());
    let kind = RowKind::of(CfaPattern::Bggr, 0);
    let m = [6554i16, -1638, -819, -1229, 6144, -819, -410, -2048, 6554];
    let rows3 = [&row16[..], &row16[..], &row16[..]];
    let rows5 = [&row16[..], &row16[..], &row16[..], &row16[..], &row16[..]];
    let mut g = c.benchmark_group("stage_1280x800");
    g.bench_function("unpack_raw10", |bn| {
        bn.iter(|| (0..H).for_each(|y| sink(simd::unpack_raw10_row(&raw[y * STRIDE..], &mut a, W))))
    });
    g.bench_function("front_black_gain", |bn| {
        bn.iter(|| {
            (0..H).for_each(|_| {
                simd::front_row(&mut a, [64, 64], &gains, 6, W);
            })
        })
    });
    g.bench_function("demosaic_bilinear", |bn| {
        bn.iter(|| {
            (0..H).for_each(|_| {
                sink(simd::demosaic_bilinear_row(
                    rows3,
                    [&mut b, &mut d, &mut e],
                    W,
                    kind,
                ))
            })
        })
    });
    g.bench_function("demosaic_mhc", |bn| {
        bn.iter(|| {
            (0..H).for_each(|_| {
                sink(simd::demosaic_mhc_row(
                    rows5,
                    [&mut b, &mut d, &mut e],
                    W,
                    kind,
                ))
            })
        })
    });
    g.bench_function("ccm", |bn| {
        bn.iter(|| {
            (0..H).for_each(|_| {
                simd::ccm_row([&mut b, &mut d, &mut e], &m, W);
            })
        })
    });
    g.bench_function("tone_lut_x3", |bn| {
        bn.iter(|| {
            (0..H).for_each(|_| {
                simd::lut_row(&b, &mut p8, &lut, W);
                simd::lut_row(&d, &mut q8, &lut, W);
                simd::lut_row(&e, &mut s8, &lut, W);
            })
        })
    });
    g.bench_function("narrow_x3", |bn| {
        bn.iter(|| {
            (0..H).for_each(|_| {
                simd::narrow_row(&b, &mut p8, W);
                simd::narrow_row(&d, &mut q8, W);
                simd::narrow_row(&e, &mut s8, W);
            })
        })
    });
    g.bench_function("interleave_rgb24", |bn| {
        bn.iter(|| {
            (0..H).for_each(|_| sink(simd::interleave_rgb_row([&p8, &q8, &s8], &mut rgb, W)))
        })
    });
    g.bench_function("rgb_to_nv12", |bn| {
        let mut uv = vec![0u8; W];
        bn.iter(|| {
            (0..H).for_each(|y| {
                simd::rgb_to_y_row([&p8, &q8, &s8], &mut rgb, W, &YuvCoeffs::BT709_LIMITED);
                if y % 2 == 1 {
                    simd::rgb_to_uv_row(
                        [&p8, &q8, &s8],
                        [&p8, &q8, &s8],
                        &mut uv,
                        &mut [],
                        W / 2,
                        &YuvCoeffs::BT709_LIMITED,
                        true,
                    );
                }
            })
        })
    });
    g.bench_function("bayer_luma", |bn| {
        bn.iter(|| {
            (0..H).for_each(|_| {
                simd::bayer_luma_row(rows3, &mut b, W);
            })
        })
    });
    g.bench_function("quad_rgb_half", |bn| {
        bn.iter(|| {
            (0..H / 2).for_each(|_| {
                sink(simd::quad_rgb_row(
                    &row16,
                    &row16,
                    [&mut b, &mut d, &mut e],
                    W / 2,
                    CfaPattern::Bggr,
                ))
            })
        })
    });
    g.finish();
    black_box((&a, &rgb));
}

/// The fp16 kernels ([`Arithmetic::Half`]; the scalar oracle without FP16 hardware).
fn half_stages(c: &mut Criterion) {
    use simd::half::{self, ColourCoeffs, ColourOut, HalfTone};
    let f = |v: usize| simd::f16::from_f64((v * 37 % 4080) as f64);
    let row: Vec<u16> = (0..W + 4).map(f).collect();
    let rows3 = [&row[..], &row[..], &row[..]];
    let raw: Vec<u16> = (0..W).map(|i| (i * 7 % 1024) as u16).collect();
    let mut front = raw.clone();
    let gains: Vec<u16> = (0..W).map(|_| simd::f16::from_f64(1.3)).collect();
    let tone = HalfTone::from_curve(|x| ToneCurve::Srgb.eval(x)).unwrap();
    let m = [[1.6, -0.4, -0.2], [-0.3, 1.5, -0.2], [-0.1, -0.5, 1.6]];
    let cc = ColourCoeffs::new(&m, RowKind::of(CfaPattern::Bggr, 0));
    let (mut p8, mut q8, mut s8) = (vec![0u8; W], vec![0u8; W], vec![0u8; W]);
    let mut rgb = vec![0u8; W * 3];
    let mut q16: [Vec<u16>; 3] = std::array::from_fn(|_| vec![0u16; W / 2]);
    let mut g = c.benchmark_group("half_1280x800");
    let (black, gain) = ([0x6440; 2], [0x4000; 2]);
    g.bench_function("front", |bn| {
        bn.iter(|| {
            (0..H).for_each(|_| {
                front.copy_from_slice(&raw);
                half::front_row(&mut front, black, gain, None, W);
            })
        })
    });
    g.bench_function("front_lsc", |bn| {
        let lsc = half::LscRow {
            a: &gains,
            d: &gains,
            t: 0x3400,
        };
        bn.iter(|| {
            (0..H).for_each(|_| {
                front.copy_from_slice(&raw);
                half::front_row(&mut front, black, gain, Some(lsc), W);
            })
        })
    });
    g.bench_function("colour_planes", |bn| {
        bn.iter(|| {
            (0..H).for_each(|_| {
                let out = ColourOut::Planes([&mut p8, &mut q8, &mut s8]);
                half::colour_row(rows3, out, W, &cc, &tone);
            })
        })
    });
    g.bench_function("colour_rgb24", |bn| {
        bn.iter(|| {
            (0..H).for_each(|_| {
                half::colour_row(rows3, ColourOut::Packed(&mut rgb), W, &cc, &tone);
            })
        })
    });
    g.bench_function("luma", |bn| {
        bn.iter(|| (0..H).for_each(|_| half::luma_row(rows3, &mut p8, W, &tone)))
    });
    g.bench_function("quad_stats_every_2nd_pair", |bn| {
        bn.iter(|| {
            (0..H / 4).for_each(|_| {
                let [r, gg, b] = &mut q16;
                half::quad_stats_row(&row, &row, [r, gg, b], W / 2, CfaPattern::Bggr, 0x3C03);
            })
        })
    });
    g.finish();
    black_box((&p8, &rgb, &front));
}

/// `set_params` with a new lens shading grid each time (32x32, as the pipeline's), and with
/// only the gains changing.
fn settings(c: &mut Criterion) {
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    for (name, arithmetic) in [("half", Arithmetic::Half), ("int", Arithmetic::Int)] {
        let base = IspParams {
            lens_shading: Some(LensShading::radial(32, 32, 0.6)),
            stats: Some(StatsConfig::default()),
            arithmetic,
            ..tuned(Demosaic::Bilinear)
        };
        let mut isp = SoftIsp::new(format, base.clone()).unwrap();
        let mut k = 0u32;
        c.bench_function(&format!("set_params/{name}_new_lsc_grid"), |bn| {
            bn.iter(|| {
                k += 1;
                let mut p = base.clone();
                p.lens_shading = Some(LensShading::radial(32, 32, 0.6 + k as f32 * 1e-4));
                isp.set_params(p).unwrap();
            })
        });
        c.bench_function(&format!("set_params/{name}_new_gains"), |bn| {
            bn.iter(|| {
                k += 1;
                let mut p = base.clone();
                p.digital_gain = 1.0 + k as f32 * 1e-4;
                isp.set_params(p).unwrap();
            })
        });
    }
}

fn run(c: &mut Criterion, name: &str, mut isp: SoftIsp, scale: Scale, kind: &str) {
    let raw = frame();
    let (ow, oh) = isp.output_size(scale);
    let (ow, oh) = (ow as usize, oh as usize);
    let (mut a, mut b) = (vec![0u8; ow * oh * 3], vec![0u8; ow * oh / 2]);
    c.bench_function(name, |bn| {
        bn.iter(|| {
            let out = match kind {
                "rgb" => OutputBuffers::Rgb24 {
                    data: &mut a,
                    stride: ow * 3,
                },
                "nv12" => OutputBuffers::Nv12 {
                    y: &mut a,
                    y_stride: ow,
                    uv: &mut b,
                    uv_stride: ow,
                },
                _ => OutputBuffers::Luma {
                    data: &mut a,
                    stride: ow,
                },
            };
            black_box(isp.process(&raw, STRIDE, scale, out).unwrap())
        })
    });
}

fn pipelines(c: &mut Criterion) {
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let isp = |p: IspParams| SoftIsp::new(format, p).unwrap();
    let int = |mut p: IspParams| {
        p.arithmetic = Arithmetic::Int;
        p
    };
    let stats = |mut p: IspParams| {
        p.stats = Some(StatsConfig::default());
        p
    };
    let shaded = |mut p: IspParams| {
        p.lens_shading = Some(LensShading::radial(16, 12, 0.6));
        p
    };
    let bilinear = || tuned(Demosaic::Bilinear);
    run(
        c,
        "e2e/rgb24_plain",
        isp(IspParams::default()),
        Scale::Full,
        "rgb",
    );
    run(c, "e2e/rgb24_tuned", isp(bilinear()), Scale::Full, "rgb");
    run(
        c,
        "e2e/rgb24_tuned_int",
        isp(int(bilinear())),
        Scale::Full,
        "rgb",
    );
    run(c, "e2e/nv12_tuned", isp(bilinear()), Scale::Full, "nv12");
    run(
        c,
        "e2e/nv12_tuned_stats",
        isp(stats(bilinear())),
        Scale::Full,
        "nv12",
    );
    run(
        c,
        "e2e/nv12_tuned_lsc_stats",
        isp(shaded(stats(bilinear()))),
        Scale::Full,
        "nv12",
    );
    run(
        c,
        "e2e/nv12_tuned_lsc_stats_int",
        isp(int(shaded(stats(bilinear())))),
        Scale::Full,
        "nv12",
    );
    run(
        c,
        "e2e/rgb24_tuned_lsc_stats",
        isp(shaded(stats(bilinear()))),
        Scale::Full,
        "rgb",
    );
    for step in [2, 4] {
        let mut p = shaded(stats(bilinear()));
        if let Some(s) = &mut p.stats {
            s.row_step = step;
        }
        run(
            c,
            &format!("e2e/rgb24_tuned_lsc_stats_step{step}"),
            isp(p),
            Scale::Full,
            "rgb",
        );
    }
    run(
        c,
        "e2e/rgb24_tuned_lsc",
        isp(shaded(bilinear())),
        Scale::Full,
        "rgb",
    );
    run(
        c,
        "e2e/nv12_tuned_lsc_stats_no_copy",
        isp(shaded(stats(bilinear()))).with_copy_input(false),
        Scale::Full,
        "nv12",
    );
    run(
        c,
        "e2e/rgb24_plain_no_copy",
        isp(IspParams::default()).with_copy_input(false),
        Scale::Full,
        "rgb",
    );
    run(
        c,
        "e2e/rgb24_tuned_mhc",
        isp(tuned(Demosaic::Mhc)),
        Scale::Full,
        "rgb",
    );
    run(
        c,
        "e2e/nv12_tuned_mhc",
        isp(tuned(Demosaic::Mhc)),
        Scale::Full,
        "nv12",
    );
    run(c, "e2e/luma_tuned", isp(bilinear()), Scale::Full, "luma");
    run(
        c,
        "e2e/luma_plain",
        isp(IspParams::default()),
        Scale::Full,
        "luma",
    );
    run(
        c,
        "e2e/rgb24_half_tuned",
        isp(bilinear()),
        Scale::Half,
        "rgb",
    );
    run(
        c,
        "e2e/nv12_half_tuned",
        isp(bilinear()),
        Scale::Half,
        "nv12",
    );
    run(
        c,
        "e2e/luma_half",
        isp(IspParams::default()),
        Scale::Half,
        "luma",
    );
    {
        let threaded = |p| isp(p).with_threads(4);
        run(
            c,
            "e2e/nv12_tuned_lsc_stats_2threads",
            isp(shaded(stats(bilinear()))).with_threads(2),
            Scale::Full,
            "nv12",
        );
        run(
            c,
            "e2e/nv12_tuned_lsc_stats_4threads",
            threaded(shaded(stats(bilinear()))),
            Scale::Full,
            "nv12",
        );
        run(
            c,
            "e2e/nv12_tuned_stats_4threads",
            threaded(stats(bilinear())),
            Scale::Full,
            "nv12",
        );
        run(
            c,
            "e2e/nv12_tuned_mhc_4threads",
            threaded(tuned(Demosaic::Mhc)),
            Scale::Full,
            "nv12",
        );
    }
}

criterion_group! {
    name = benches;
    config = configure();
    targets = stages, half_stages, settings, pipelines
}
criterion_main!(benches);
