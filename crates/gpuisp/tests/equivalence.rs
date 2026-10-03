//! The GPU ISP against `styx-softisp`'s integer arithmetic, bit for bit: every output, scale,
//! demosaic, colour pattern and packing, with and without lens shading, colour matrix and
//! tone curve, and the statistics, on every Vulkan device of the machine (RADV and llvmpipe
//! here). Without Vulkan the tests pass with a note.

mod common;

use common::{PACKINGS, PATTERNS, all_outputs, contexts, mosaic, pack};
use styx_gpuisp::{GpuContext, GpuIsp};
use styx_softisp::*;

/// Gradients, fine stripes, a hard edge, clipped highlights and noise, `bits`-bit.
fn frame(w: usize, h: usize, pattern: CfaPattern, bits: u8) -> Vec<u16> {
    let max = (1u32 << bits) - 1;
    mosaic(w, h, pattern, |x, y| {
        let mut seed = (x as u32 * 7919 + y as u32 * 104_729) ^ 0x2545_F491;
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let noise = (seed & 31) as i32 - 16;
        let base = [
            (x * 1023 / w) as i32,
            ((x + y) * 7 % 900) as i32 + if (x / 3) % 2 == 0 { 60 } else { 0 },
            if x > w / 2 { 900 } else { 120 + (y * 9) as i32 },
        ];
        let clip = if (100..120).contains(&x) && y < 30 {
            1023
        } else {
            0
        };
        base.map(|v| {
            let v = (v + noise).max(clip).clamp(0, 1023) as u32;
            (v * max / 1023) as u16
        })
    })
}

fn params(demosaic: Demosaic, shaded: bool, full: bool, bins: u32, row_step: u32) -> IspParams {
    let mut p = IspParams {
        black_level: Some(BlackLevel {
            r: 64,
            gr: 60,
            gb: 66,
            b: 70,
        }),
        white_balance: Some(WhiteBalance {
            r: 1.9,
            g: 1.0,
            b: 1.6,
        }),
        digital_gain: 1.3,
        lens_shading: shaded.then(|| LensShading::radial(9, 7, 0.7)),
        demosaic,
        stats: Some(StatsConfig {
            zones_x: 8,
            zones_y: 6,
            histogram_bins: bins,
            saturation: 0.95,
            row_step,
            ..StatsConfig::default()
        }),
        arithmetic: Arithmetic::Int,
        ..IspParams::default()
    };
    if full {
        p.ccm = Some(ColorMatrix {
            m: [[1.6, -0.4, -0.2], [-0.3, 1.5, -0.2], [-0.1, -0.5, 1.6]],
        });
        p.tone = Some(ToneCurve::Points {
            points: vec![
                [0.0, 0.0],
                [0.05, 0.2],
                [0.25, 0.55],
                [0.6, 0.85],
                [1.0, 1.0],
            ],
        });
        p.yuv = YuvMatrix::Bt601Full;
    }
    p
}

/// Every output of both ISPs on one frame, compared; the statistics too.
fn check(ctx: &GpuContext, format: RawFormat, params: &IspParams, raw: &[u8], stride: usize) {
    let mut cpu = SoftIsp::new(format, params.clone()).unwrap();
    let mut gpu = GpuIsp::with_context(ctx, format, params.clone()).unwrap();
    for scale in [Scale::Full, Scale::Half] {
        let (w, h) = cpu.output_size(scale);
        let size = (w as usize, h as usize);
        let (want, want_stats) =
            all_outputs(size, &mut |o| cpu.process(raw, stride, scale, o).unwrap());
        let (got, got_stats) =
            all_outputs(size, &mut |o| gpu.process(raw, stride, scale, o).unwrap());
        for ((name, a), (_, b)) in want.iter().zip(&got) {
            if a != b {
                let at = a.iter().zip(b).position(|(p, q)| p != q).unwrap_or(0);
                panic!(
                    "{}: {name} differs at byte {at} ({} vs {}), {format:?} {scale:?} {:?}",
                    ctx.info().name,
                    a[at],
                    b[at],
                    params.demosaic
                );
            }
        }
        assert_eq!(
            want_stats,
            got_stats,
            "{} statistics, {format:?}",
            ctx.info().name
        );
    }
}

#[test]
fn every_output_bit_exact() {
    for ctx in contexts() {
        for pattern in PATTERNS {
            let (w, h) = (264, 100);
            let m = frame(w, h, pattern, 10);
            let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
            let format = RawFormat::new(w as u32, h as u32, pattern, RawPacking::Csi2Raw10);
            for demosaic in [Demosaic::Bilinear, Demosaic::Mhc] {
                for shaded in [false, true] {
                    let p = params(demosaic, shaded, true, 64, 1);
                    check(&ctx, format, &p, &raw, stride);
                }
            }
        }
    }
}

#[test]
fn every_packing_bit_exact() {
    for ctx in contexts() {
        for packing in PACKINGS {
            let (w, h) = (200, 64);
            let m = frame(w, h, CfaPattern::Grbg, packing.bit_depth());
            let (raw, stride) = pack(&m, w, h, packing);
            let format = RawFormat::new(w as u32, h as u32, CfaPattern::Grbg, packing);
            let mut p = params(Demosaic::Bilinear, true, true, 256, 3);
            let full = (1u32 << packing.bit_depth()) - 1;
            p.black_level = Some(BlackLevel::uniform((full / 16) as u16));
            check(&ctx, format, &p, &raw, stride);
        }
    }
}

/// No colour matrix and no tone curve (plain narrowing), BT.709 limited range, a wide
/// histogram (kept in global memory), sparse statistics rows, and sizes that are not
/// multiples of the workgroup tiles (odd half sizes too).
#[test]
fn plain_settings_and_odd_sizes() {
    for ctx in contexts() {
        for (w, h) in [(262, 98), (70, 34), (4, 4)] {
            let m = frame(w, h, CfaPattern::Bggr, 10);
            let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
            let format =
                RawFormat::new(w as u32, h as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
            let mut p = params(Demosaic::Bilinear, w > 4, false, 2048, 5);
            if let Some(s) = &mut p.stats {
                s.zones_x = s.zones_x.min(w as u32 / 2);
                s.zones_y = s.zones_y.min(h as u32 / 2);
            }
            check(&ctx, format, &p, &raw, stride);
            check(&ctx, format, &IspParams::default(), &raw, stride);
        }
    }
}

/// Parameters change between frames (as the 3A loop changes them) and statistics can be
/// skipped.
#[test]
fn parameter_changes_and_skipped_statistics() {
    for ctx in contexts() {
        let (w, h) = (128, 64);
        let m = frame(w, h, CfaPattern::Rggb, 10);
        let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
        let format = RawFormat::new(w as u32, h as u32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
        let mut gpu = GpuIsp::with_context(&ctx, format, IspParams::default()).unwrap();
        for k in 0..4 {
            let mut p = params(Demosaic::Bilinear, k % 2 == 1, k > 0, 128, 1);
            p.digital_gain = 1.0 + k as f32 * 0.25;
            gpu.set_params(p.clone()).unwrap();
            gpu.set_statistics(k != 2);
            let mut cpu = SoftIsp::new(format, p).unwrap();
            cpu.set_statistics(k != 2);
            let (mut a, mut b) = (vec![0u8; w * h * 3], vec![0u8; w * h * 3]);
            let sa = cpu
                .process(
                    &raw,
                    stride,
                    Scale::Full,
                    OutputBuffers::Rgb24 {
                        data: &mut a,
                        stride: w * 3,
                    },
                )
                .unwrap();
            let sb = gpu
                .process(
                    &raw,
                    stride,
                    Scale::Full,
                    OutputBuffers::Rgb24 {
                        data: &mut b,
                        stride: w * 3,
                    },
                )
                .unwrap();
            assert!(a == b, "frame {k}");
            assert_eq!(sa, sb, "frame {k}");
            assert_eq!(sb.is_none(), k == 2);
        }
    }
}

/// The checks `styx-softisp` makes, with the same errors.
#[test]
fn errors_match_softisp() {
    for ctx in contexts() {
        let format = RawFormat::new(64, 32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
        let bad = IspParams {
            digital_gain: f32::NAN,
            ..IspParams::default()
        };
        let e = GpuIsp::with_context(&ctx, format, bad.clone()).unwrap_err();
        let want = SoftIsp::new(format, bad).unwrap_err();
        assert_eq!(e.to_string(), want.to_string());
        let mut gpu = GpuIsp::with_context(&ctx, format, IspParams::default()).unwrap();
        let mut cpu = SoftIsp::new(format, IspParams::default()).unwrap();
        let raw = vec![0u8; 80 * 31];
        let mut rgb = vec![0u8; 64 * 32 * 3];
        let out = || OutputBuffers::Rgb24 {
            data: &mut [],
            stride: 64 * 3,
        };
        let a = gpu.process(&raw, 80, Scale::Full, out()).unwrap_err();
        let b = cpu.process(&raw, 80, Scale::Full, out()).unwrap_err();
        assert_eq!(a.to_string(), b.to_string());
        let a = gpu
            .process(
                &raw,
                80,
                Scale::Full,
                OutputBuffers::Rgb24 {
                    data: &mut rgb,
                    stride: 192,
                },
            )
            .unwrap_err();
        let b = cpu
            .process(
                &raw,
                80,
                Scale::Full,
                OutputBuffers::Rgb24 {
                    data: &mut rgb,
                    stride: 192,
                },
            )
            .unwrap_err();
        assert_eq!(a.to_string(), b.to_string());
    }
}
