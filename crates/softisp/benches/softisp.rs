//! Software ISP on a 1280x800 RAW10 frame (the OV9782's `pBAA`, stride 1600): each stage over
//! a whole frame's rows, and whole pipelines end to end.
//!
//! `cargo bench -p styx-softisp` (add `--features rayon` for the threaded rows).

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
        .sample_size(30)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2))
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
    let m = [1638i16, -410, -205, -307, 1536, -205, -102, -512, 1638];
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
    if cfg!(feature = "rayon") {
        let threaded = |p| isp(p).with_threads(4);
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
    targets = stages, pipelines
}
criterion_main!(benches);
