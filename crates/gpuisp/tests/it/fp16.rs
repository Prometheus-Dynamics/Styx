//! The fp16 pictures (`GpuIsp::set_half_precision`) against `styx-softisp`'s `Arithmetic::Half`,
//! within its own bounds (PSNR above 45 dB, at most 6 codes, 99.9% within 2; `quality.rs`): the
//! sRGB and contrast curves, with and without lens shading, at both scales. Skipped with a note
//! where a device lacks `shaderFloat16`.

use crate::common::{compare, contexts};
use crate::quality::{H, W, chart, contrast_curve, gpu_outputs, outputs, params};
use styx_gpuisp::GpuIsp;
use styx_softisp::*;

#[test]
fn fp16_pictures_against_half() {
    let print = std::env::var_os("STYX_QUALITY_PRINT").is_some();
    for ctx in contexts() {
        if !ctx.info().float16 {
            eprintln!(
                "{}: no shaderFloat16, fp16 pictures skipped",
                ctx.info().name
            );
            continue;
        }
        for pattern in [CfaPattern::Bggr, CfaPattern::Grbg] {
            let (raw, stride) = chart(pattern);
            let format = RawFormat::new(W as u32, H as u32, pattern, RawPacking::Csi2Raw10);
            for (curve, name) in [(ToneCurve::Srgb, "sRGB"), (contrast_curve(), "contrast")] {
                for shaded in [false, true] {
                    let p = params(curve.clone(), shaded, Arithmetic::Half);
                    let mut gpu = GpuIsp::with_context(&ctx, format, p).unwrap();
                    gpu.set_half_precision(true);
                    assert_eq!(gpu.arithmetic(), Arithmetic::Half, "{}", ctx.info().name);
                    for scale in [Scale::Full, Scale::Half] {
                        let g = gpu_outputs(&mut gpu, &raw, stride, scale);
                        let mut cpu =
                            SoftIsp::new(format, params(curve.clone(), shaded, Arithmetic::Half))
                                .unwrap();
                        assert_eq!(cpu.arithmetic(), Arithmetic::Half);
                        let c = outputs(&mut cpu, &raw, stride, scale);
                        let luma = match scale {
                            Scale::Full => W * H,
                            Scale::Half => W * H / 4,
                        };
                        let figures = [
                            ("R", compare(&c.0, &g.0, 0, 3)),
                            ("G", compare(&c.0, &g.0, 1, 3)),
                            ("B", compare(&c.0, &g.0, 2, 3)),
                            ("Y", compare(&c.1[..luma], &g.1[..luma], 0, 1)),
                            ("U", compare(&c.1[luma..], &g.1[luma..], 0, 2)),
                            ("V", compare(&c.1[luma..], &g.1[luma..], 1, 2)),
                        ];
                        let case = format!(
                            "{}: {pattern:?} {name} shaded {shaded} {scale:?} fp16 vs Half",
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
                            let ok = psnr > 45.0 && max <= 6 && over < 0.001;
                            assert!(ok, "{case} {c}: {psnr:.2} dB, max {max}, {over} beyond 2");
                        }
                    }
                }
            }
        }
    }
}
