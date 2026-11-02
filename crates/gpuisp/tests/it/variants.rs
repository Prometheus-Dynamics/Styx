//! The edge-directed demosaic and the tone curve as quadratics (`IntPolyTone`) on the GPU
//! against `styx-softisp`, bit for bit, on every Vulkan device. Without Vulkan the tests pass
//! with a note.

use crate::common::{PATTERNS, all_outputs, contexts, pack};
use crate::equivalence::{check, frame, params};
use styx_gpuisp::GpuIsp;
use styx_softisp::*;

#[test]
fn edge_directed_bit_exact() {
    for ctx in contexts() {
        for pattern in PATTERNS {
            for (w, h) in [(264, 100), (70, 34)] {
                let m = frame(w, h, pattern, 10);
                let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
                let format = RawFormat::new(w as u32, h as u32, pattern, RawPacking::Csi2Raw10);
                for (full, shaded) in [(false, false), (true, false), (true, true)] {
                    let p = params(Demosaic::EdgeDirected, shaded, full, 64, 1);
                    check(&ctx, format, &p, &raw, stride);
                }
            }
        }
    }
}

/// Bilinear demosaic, colour matrix and `tone` as quadratics (`IntPolyTone`).
fn poly_params(tone: ToneCurve, shaded: bool) -> IspParams {
    let mut p = params(Demosaic::Bilinear, shaded, true, 64, 1);
    p.tone = Some(tone);
    p.arithmetic = Arithmetic::IntPolyTone;
    p
}

#[test]
fn polynomial_tone_bit_exact() {
    for ctx in contexts() {
        for pattern in [CfaPattern::Bggr, CfaPattern::Rggb] {
            let (w, h) = (264, 100);
            let m = frame(w, h, pattern, 10);
            let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
            let format = RawFormat::new(w as u32, h as u32, pattern, RawPacking::Csi2Raw10);
            for tone in [
                ToneCurve::Srgb,
                ToneCurve::Gamma { gamma: 2.2 },
                ToneCurve::Gamma { gamma: 1.8 },
            ] {
                for shaded in [false, true] {
                    let p = poly_params(tone.clone(), shaded);
                    let cpu = SoftIsp::new(format, p.clone()).unwrap();
                    assert_eq!(cpu.arithmetic(), Arithmetic::IntPolyTone, "{tone:?}");
                    let gpu = GpuIsp::with_context(&ctx, format, p.clone()).unwrap();
                    assert_eq!(gpu.arithmetic(), Arithmetic::IntPolyTone, "{tone:?}");
                    check(&ctx, format, &p, &raw, stride);
                }
            }
        }
    }
}

/// A curve the quadratics miss by more than a code (`out = in^5`): both keep the table.
#[test]
fn polynomial_tone_falls_back_to_the_table() {
    for ctx in contexts() {
        let (w, h) = (128, 64);
        let m = frame(w, h, CfaPattern::Grbg, 10);
        let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
        let format = RawFormat::new(w as u32, h as u32, CfaPattern::Grbg, RawPacking::Csi2Raw10);
        let p = poly_params(ToneCurve::Gamma { gamma: 0.2 }, false);
        let cpu = SoftIsp::new(format, p.clone()).unwrap();
        assert_eq!(cpu.arithmetic(), Arithmetic::Int);
        let gpu = GpuIsp::with_context(&ctx, format, p.clone()).unwrap();
        assert_eq!(gpu.arithmetic(), Arithmetic::Int);
        check(&ctx, format, &p, &raw, stride);
    }
}

/// The tone mode changes between frames (table, quadratics, another curve, the same curve
/// again): every frame matches a fresh CPU ISP.
#[test]
fn tone_mode_changes_between_frames() {
    for ctx in contexts() {
        let (w, h) = (128, 64);
        let m = frame(w, h, CfaPattern::Rggb, 10);
        let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
        let format = RawFormat::new(w as u32, h as u32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
        let mut table = poly_params(ToneCurve::Srgb, false);
        table.arithmetic = Arithmetic::Int;
        let steps = [
            poly_params(ToneCurve::Srgb, false),
            table,
            poly_params(ToneCurve::Gamma { gamma: 2.2 }, true),
            poly_params(ToneCurve::Gamma { gamma: 2.2 }, true),
            poly_params(ToneCurve::Srgb, false),
        ];
        let mut gpu = GpuIsp::with_context(&ctx, format, IspParams::default()).unwrap();
        for p in steps {
            gpu.set_params(p.clone()).unwrap();
            let mut cpu = SoftIsp::new(format, p.clone()).unwrap();
            let (want, _) = all_outputs((w, h), &mut |o| {
                cpu.process(&raw, stride, Scale::Full, o).unwrap()
            });
            let (got, _) = all_outputs((w, h), &mut |o| {
                gpu.process(&raw, stride, Scale::Full, o).unwrap()
            });
            assert_eq!(want, got, "{}: {:?}", ctx.info().name, p.arithmetic);
        }
    }
}
