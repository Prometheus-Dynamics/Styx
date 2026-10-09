//! The GPU ISP against `styx-softisp` on a synthetic chart (colour patches, grey ramps, fine
//! detail, clipped highlights, noise; `styx-softisp`'s quality chart) with the sRGB curve and a
//! Raspberry Pi contrast curve, with and without lens shading, at both scales: identical to
//! the integer arithmetic, within one code of its tone quadratics (`IntPolyTone`, what x86
//! runs by default) and within `styx-softisp`'s own bounds of its fp16 arithmetic (PSNR above
//! 45 dB, 99.9% of the samples within 2 codes; what the Cortex-A76 runs by default).
//!
//! `STYX_QUALITY_PRINT=1 cargo test -p styx-gpuisp --release --test it quality -- --nocapture`
//! prints the figures.

use crate::common::{compare, contexts, mosaic, pack};
use styx_gpuisp::GpuIsp;
use styx_softisp::*;

const W: usize = 320;
const H: usize = 240;

/// Colour patches (6x4), a grey ramp band, a zone plate and a clipped corner, 10-bit, with
/// noise.
fn chart(pattern: CfaPattern) -> (Vec<u8>, usize) {
    let m = mosaic(W, H, pattern, |x, y| {
        let mut seed = (x as u32 * 7919 + y as u32 * 104_729) ^ 0x2545_F491;
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let noise = (seed % 9) as i32 - 4;
        let rgb: [i32; 3] = if y < 160 {
            let (px, py) = (x * 6 / W, y / 40);
            let k = (px + py * 6) as i32;
            [
                64 + (k * 97) % 900,
                64 + (k * 53 + 300) % 900,
                64 + (k * 31 + 600) % 900,
            ]
        } else if y < 200 {
            let v = 64 + (x * 959 / W) as i32;
            [v, v, v]
        } else if x < W - 40 {
            let r2 = ((x * x + (y - 200) * (y - 200)) as f32) / 50.0;
            let v = (500.0 + 400.0 * r2.sin()) as i32;
            [v, v, v]
        } else {
            [1023, 1023, 1023]
        };
        rgb.map(|v| (v + noise).clamp(0, 1023) as u16)
    });
    pack(&m, W, H, RawPacking::Csi2Raw10)
}

fn params(tone: ToneCurve, shaded: bool, arithmetic: Arithmetic) -> IspParams {
    IspParams {
        black_level: Some(BlackLevel::uniform(64)),
        white_balance: Some(WhiteBalance {
            r: 1.9,
            g: 1.0,
            b: 1.6,
        }),
        digital_gain: 1.2,
        lens_shading: shaded.then(shading),
        ccm: Some(ColorMatrix {
            m: [[1.6, -0.4, -0.2], [-0.3, 1.5, -0.2], [-0.1, -0.5, 1.6]],
        }),
        tone: Some(tone),
        yuv: YuvMatrix::Bt601Full,
        arithmetic,
        ..Default::default()
    }
}

/// Radial falloff, different per channel and lopsided (as calibrated tables are).
fn shading() -> LensShading {
    let mut ls = LensShading::radial(32, 32, 0.6);
    for (i, (r, b)) in ls.r.iter_mut().zip(ls.b.iter_mut()).enumerate() {
        let (x, y) = ((i % 32) as f32 / 31.0, (i / 32) as f32 / 31.0);
        *r *= 1.0 + 0.3 * x;
        *b *= 1.2 - 0.3 * y;
    }
    ls
}

/// The HeliOS tuning's (Raspberry Pi) contrast curve, 16-bit points.
fn contrast_curve() -> ToneCurve {
    const P: [u32; 64] = [
        0, 0, 512, 2518, 1024, 5033, 1536, 7175, 2048, 9309, 2560, 10814, 3072, 12312, 3584, 13773,
        4096, 15225, 4608, 16566, 5120, 17899, 5632, 19221, 6144, 20534, 6656, 21684, 7168, 22826,
        7680, 24024, 8192, 25212, 9216, 27251, 10240, 29167, 11264, 30947, 12288, 32696, 13312,
        34309, 14336, 35849, 15360, 37194, 16384, 38445, 20480, 42687, 24576, 45871, 32768, 50470,
        40960, 54294, 49152, 57824, 57344, 61558, 65535, 65535,
    ];
    ToneCurve::Points {
        points: P
            .chunks(2)
            .map(|p| [p[0] as f32 / 65535.0, p[1] as f32 / 65535.0])
            .collect(),
    }
}

fn outputs(isp: &mut SoftIsp, raw: &[u8], stride: usize, scale: Scale) -> (Vec<u8>, Vec<u8>) {
    let (w, h) = isp.output_size(scale);
    let (w, h) = (w as usize, h as usize);
    let mut rgb = vec![0u8; w * h * 3];
    let out = OutputBuffers::Rgb24 {
        data: &mut rgb,
        stride: w * 3,
    };
    isp.process(raw, stride, scale, out).unwrap();
    let mut nv12 = vec![0u8; w * h * 3 / 2];
    let (y, uv) = nv12.split_at_mut(w * h);
    let out = OutputBuffers::Nv12 {
        y,
        y_stride: w,
        uv,
        uv_stride: w,
    };
    isp.process(raw, stride, scale, out).unwrap();
    (rgb, nv12)
}

/// RGB24 and NV12 of the GPU ISP.
fn gpu_outputs(isp: &mut GpuIsp, raw: &[u8], stride: usize, scale: Scale) -> (Vec<u8>, Vec<u8>) {
    let (w, h) = isp.output_size(scale);
    let (w, h) = (w as usize, h as usize);
    let mut rgb = vec![0u8; w * h * 3];
    let out = OutputBuffers::Rgb24 {
        data: &mut rgb,
        stride: w * 3,
    };
    isp.process(raw, stride, scale, out).unwrap();
    let mut nv12 = vec![0u8; w * h * 3 / 2];
    let (y, uv) = nv12.split_at_mut(w * h);
    let out = OutputBuffers::Nv12 {
        y,
        y_stride: w,
        uv,
        uv_stride: w,
    };
    isp.process(raw, stride, scale, out).unwrap();
    (rgb, nv12)
}

#[test]
fn gpu_against_every_arithmetic() {
    let print = std::env::var_os("STYX_QUALITY_PRINT").is_some();
    for ctx in contexts() {
        for pattern in [CfaPattern::Bggr, CfaPattern::Grbg] {
            let (raw, stride) = chart(pattern);
            let format = RawFormat::new(W as u32, H as u32, pattern, RawPacking::Csi2Raw10);
            for (curve, name) in [(ToneCurve::Srgb, "sRGB"), (contrast_curve(), "contrast")] {
                for shaded in [false, true] {
                    for scale in [Scale::Full, Scale::Half] {
                        let p = params(curve.clone(), shaded, Arithmetic::Int);
                        let mut gpu = GpuIsp::with_context(&ctx, format, p).unwrap();
                        let g = gpu_outputs(&mut gpu, &raw, stride, scale);
                        let luma = match scale {
                            Scale::Full => W * H,
                            Scale::Half => W * H / 4,
                        };
                        let arithmetics = [
                            Arithmetic::Int,
                            Arithmetic::IntPolyTone,
                            Arithmetic::Half,
                            Arithmetic::Auto,
                        ];
                        for arithmetic in arithmetics {
                            let mut cpu =
                                SoftIsp::new(format, params(curve.clone(), shaded, arithmetic))
                                    .unwrap();
                            let resolved = cpu.arithmetic();
                            let c = outputs(&mut cpu, &raw, stride, scale);
                            let figures = [
                                ("R", compare(&c.0, &g.0, 0, 3)),
                                ("G", compare(&c.0, &g.0, 1, 3)),
                                ("B", compare(&c.0, &g.0, 2, 3)),
                                ("Y", compare(&c.1[..luma], &g.1[..luma], 0, 1)),
                                ("U", compare(&c.1[luma..], &g.1[luma..], 0, 2)),
                                ("V", compare(&c.1[luma..], &g.1[luma..], 1, 2)),
                            ];
                            let case = format!(
                                "{}: {pattern:?} {name} shaded {shaded} {scale:?} vs {arithmetic:?} ({resolved:?})",
                                ctx.info().name
                            );
                            if print {
                                let f: Vec<String> = figures
                                    .iter()
                                    .map(|(c, (p, m, o))| {
                                        format!("{c} {p:.1} dB max {m} ({:.3}% > 2)", 100.0 * o)
                                    })
                                    .collect();
                                println!("{case}: {}", f.join(", "));
                            }
                            for (c, (psnr, max, over)) in figures {
                                let ok = match resolved {
                                    Arithmetic::Int => max == 0,
                                    Arithmetic::IntPolyTone => max <= 1,
                                    _ => psnr > 45.0 && max <= 6 && over < 0.001,
                                };
                                assert!(ok, "{case} {c}: {psnr:.2} dB, max {max}, {over} beyond 2");
                            }
                        }
                    }
                }
            }
        }
    }
}
