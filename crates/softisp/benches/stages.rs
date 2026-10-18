//! Per-frame cost of the quality stages on a 1280x800 RAW10 frame, one thread: the demosaic
//! variants, and the output stages (luma denoise, chroma denoise, sharpening, all of them) on
//! top of the bilinear, tone-mapped, colour-corrected baseline. Each stage's cost is its
//! configuration's time minus the baseline's (the same frame, the same integer arithmetic).
//!
//! `cargo bench -p styx-softisp --bench stages`. Host numbers are relative: the CM5 (Cortex-A76)
//! is the target.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use styx_softisp::*;

const W: usize = 1280;
const H: usize = 800;
const STRIDE: usize = 1600;

/// A textured BGGR RAW10 frame: gradients plus pseudo-random noise (as the `softisp` bench).
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

/// The baseline: bilinear, colour matrix, sRGB tone, integer arithmetic.
fn base(demosaic: Demosaic) -> IspParams {
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
        arithmetic: Arithmetic::Int,
        ..Default::default()
    }
}

fn luma_denoise() -> Denoise {
    Denoise {
        luma: Some(LumaDenoise {
            constant: 0.0,
            slope: 9.6,
            strength: 0.25,
        }),
        chroma: None,
    }
}

fn chroma_denoise() -> Denoise {
    Denoise {
        luma: None,
        chroma: Some(ChromaDenoise {
            threshold: 450.0,
            strength: 0.2,
        }),
    }
}

fn sharpen() -> Sharpen {
    Sharpen {
        amount: 0.5,
        threshold: 2.0,
        limit: 8.0,
    }
}

fn bench_stages(c: &mut Criterion) {
    let raw = frame();
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let mut rgb = vec![0u8; 3 * W * H];
    let configs: Vec<(&str, IspParams)> = vec![
        ("baseline bilinear", base(Demosaic::Bilinear)),
        ("demosaic mhc", base(Demosaic::Mhc)),
        ("demosaic edge-directed", base(Demosaic::EdgeDirected)),
        (
            "luma denoise",
            IspParams {
                denoise: Some(luma_denoise()),
                ..base(Demosaic::Bilinear)
            },
        ),
        (
            "chroma denoise",
            IspParams {
                denoise: Some(chroma_denoise()),
                ..base(Demosaic::Bilinear)
            },
        ),
        (
            "output path (sharpen at amount 0)",
            IspParams {
                sharpen: Some(Sharpen {
                    amount: 0.0,
                    threshold: 2.0,
                    limit: 8.0,
                }),
                ..base(Demosaic::Bilinear)
            },
        ),
        (
            "sharpen",
            IspParams {
                sharpen: Some(sharpen()),
                ..base(Demosaic::Bilinear)
            },
        ),
        (
            "all output stages",
            IspParams {
                denoise: Some(Denoise {
                    luma: luma_denoise().luma,
                    chroma: chroma_denoise().chroma,
                }),
                sharpen: Some(sharpen()),
                ..base(Demosaic::Bilinear)
            },
        ),
    ];
    let mut group = c.benchmark_group("softisp_stages_1280x800");
    group.sample_size(20);
    for (name, params) in configs {
        let mut isp = SoftIsp::new(format, params).unwrap();
        group.bench_function(name, |b| {
            b.iter(|| {
                let out = isp
                    .process(
                        &raw,
                        STRIDE,
                        Scale::Full,
                        OutputBuffers::Rgb24 {
                            data: &mut rgb,
                            stride: 3 * W,
                        },
                    )
                    .unwrap();
                black_box(out);
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_stages);
criterion_main!(benches);
