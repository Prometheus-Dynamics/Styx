//! [`Arithmetic::Half`] against the integer reference on a synthetic chart (colour patches,
//! grey ramps, fine detail, clipped highlights, noise): PSNR above 45 dB and at most 2 codes
//! apart on 99.9% of the samples of every channel of RGB24 and NV12, at both scales, with and
//! without lens shading, for the sRGB curve and a Raspberry Pi contrast curve. The rest (up to
//! 6 codes) are pixels where the colour matrix subtracts a clipped channel from a dark one:
//! fp16 rounds the large terms to 1-4 units, the integer path to its own half unit, and sRGB's
//! steep start magnifies either. On machines without FP16 arithmetic
//! the fp16 side is the scalar oracle, which the FP16 leaves match bit for bit.
//!
//! `STYX_QUALITY_PRINT=1 cargo test -p styx-softisp --release --test it quality -- --nocapture`
//! prints the figures.

use crate::common::{mosaic, pack};
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

/// PSNR (dB), largest difference and the share of samples more than 2 codes apart, of
/// interleaved channel `c` of `n`.
fn compare(a: &[u8], b: &[u8], c: usize, n: usize) -> (f64, u8, f64) {
    let (mut sq, mut max, mut k, mut over) = (0f64, 0u8, 0usize, 0usize);
    for (p, q) in a.iter().skip(c).step_by(n).zip(b.iter().skip(c).step_by(n)) {
        let d = p.abs_diff(*q);
        sq += f64::from(d) * f64::from(d);
        max = max.max(d);
        over += usize::from(d > 2);
        k += 1;
    }
    let mse = sq / k.max(1) as f64;
    let psnr = if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    };
    (psnr, max, over as f64 / k.max(1) as f64)
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

#[test]
fn half_is_within_two_codes_of_int() {
    let print = std::env::var_os("STYX_QUALITY_PRINT").is_some();
    for pattern in [CfaPattern::Bggr, CfaPattern::Rggb, CfaPattern::Gbrg] {
        let (raw, stride) = chart(pattern);
        let format = RawFormat::new(W as u32, H as u32, pattern, RawPacking::Csi2Raw10);
        for (curve, name) in [(ToneCurve::Srgb, "sRGB"), (contrast_curve(), "contrast")] {
            for shaded in [false, true] {
                for scale in [Scale::Full, Scale::Half] {
                    let run = |arithmetic| {
                        let p = params(curve.clone(), shaded, arithmetic);
                        let mut isp = SoftIsp::new(format, p).unwrap();
                        outputs(&mut isp, &raw, stride, scale)
                    };
                    let (int, half) = (run(Arithmetic::Int), run(Arithmetic::Half));
                    let (w, h) = (W as u32, H as u32);
                    let luma = match scale {
                        Scale::Full => (w * h) as usize,
                        Scale::Half => (w * h / 4) as usize,
                    };
                    let figures = [
                        ("R", compare(&int.0, &half.0, 0, 3)),
                        ("G", compare(&int.0, &half.0, 1, 3)),
                        ("B", compare(&int.0, &half.0, 2, 3)),
                        ("Y", compare(&int.1[..luma], &half.1[..luma], 0, 1)),
                        ("U", compare(&int.1[luma..], &half.1[luma..], 0, 2)),
                        ("V", compare(&int.1[luma..], &half.1[luma..], 1, 2)),
                    ];
                    let case = format!("{pattern:?} {name} shaded {shaded} {scale:?}");
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
                        assert!(
                            psnr > 45.0 && max <= 6 && over < 0.001,
                            "{case} {c}: {psnr:.2} dB, max {max}, {over} beyond 2"
                        );
                    }
                }
            }
        }
    }
}

/// [`Arithmetic::IntPolyTone`] against [`Arithmetic::Int`]: the same but for the tone curve,
/// whose quadratics land within one code of the table, so every output sample (RGB24, NV12,
/// luma) is within one code, and most are equal.
#[test]
fn poly_tone_is_within_one_code_of_int() {
    let print = std::env::var_os("STYX_QUALITY_PRINT").is_some();
    for pattern in [CfaPattern::Bggr, CfaPattern::Grbg] {
        let (raw, stride) = chart(pattern);
        let format = RawFormat::new(W as u32, H as u32, pattern, RawPacking::Csi2Raw10);
        let curves = [
            (ToneCurve::Srgb, "sRGB"),
            (contrast_curve(), "contrast"),
            (ToneCurve::Gamma { gamma: 2.2 }, "gamma 2.2"),
        ];
        for (curve, name) in curves {
            for scale in [Scale::Full, Scale::Half] {
                let run = |arithmetic| {
                    let mut isp =
                        SoftIsp::new(format, params(curve.clone(), true, arithmetic)).unwrap();
                    let (rgb, nv12) = outputs(&mut isp, &raw, stride, scale);
                    let (w, h) = isp.output_size(scale);
                    let mut luma = vec![0u8; (w * h) as usize];
                    let out = OutputBuffers::Luma {
                        data: &mut luma,
                        stride: w as usize,
                    };
                    isp.process(&raw, stride, scale, out).unwrap();
                    (isp.arithmetic(), [rgb, nv12, luma])
                };
                let (a, int) = run(Arithmetic::Int);
                let (b, poly) = run(Arithmetic::IntPolyTone);
                assert_eq!((a, b), (Arithmetic::Int, Arithmetic::IntPolyTone), "{name}");
                for (kind, (i, p)) in ["RGB24", "NV12", "luma"].iter().zip(int.iter().zip(&poly)) {
                    let (psnr, max, _) = compare(i, p, 0, 1);
                    let equal = i.iter().zip(p).filter(|(x, y)| x == y).count();
                    let equal = equal as f64 / i.len() as f64;
                    if print {
                        println!(
                            "{pattern:?} {name} {scale:?} {kind}: {psnr:.1} dB, max {max}, {:.1}% equal",
                            100.0 * equal
                        );
                    }
                    assert!(
                        max <= 1 && equal > 0.75,
                        "{pattern:?} {name} {scale:?} {kind}"
                    );
                }
            }
        }
    }
}
