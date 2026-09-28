//! Whole-pipeline tests on synthetic mosaics: known colours come back as the right RGB, YUV and
//! luma for every CFA pattern and packing, at both scales and with both demosaics.

mod common;

use common::*;
use styx_softisp::simd::{YuvCoeffs, scalar};
use styx_softisp::*;

const W: usize = 64;
const H: usize = 48;

/// A flat colour given in 8-bit terms, as raw samples of `bits` (low bits set, which the 8-bit
/// output truncates).
fn raw_colour(c8: [u16; 3], bits: u8) -> [u16; 3] {
    let s = bits - 8;
    c8.map(|c| (c << s) | ((1 << s) - 1))
}

#[test]
fn flat_colours_round_trip_for_every_pattern_packing_and_demosaic() {
    let colours = [[200, 120, 40], [0, 0, 0], [255, 255, 255], [17, 250, 99]];
    for pattern in PATTERNS {
        for packing in PACKINGS {
            for c8 in colours {
                let raw = raw_colour(c8, packing.bit_depth());
                let m = mosaic(W, H, pattern, |_, _| raw);
                let (bytes, stride) = pack(&m, W, H, packing);
                let format = RawFormat::new(W as u32, H as u32, pattern, packing);
                for demosaic in [Demosaic::Bilinear, Demosaic::Mhc] {
                    let params = IspParams {
                        demosaic,
                        ..Default::default()
                    };
                    let (out, _) = rgb(format, &params, &bytes, stride, Scale::Full);
                    let want = c8.map(|c| c as u8);
                    for px in out.chunks_exact(3) {
                        assert_eq!(px, want, "{pattern:?} {packing:?} {demosaic:?} {c8:?}");
                    }
                }
                let (half, _) = rgb(format, &IspParams::default(), &bytes, stride, Scale::Half);
                assert_eq!(half.len(), W * H * 3 / 4);
                assert!(
                    half.chunks_exact(3).all(|px| px == c8.map(|c| c as u8)),
                    "half {pattern:?} {packing:?}"
                );
            }
        }
    }
}

#[test]
fn black_level_and_white_balance_scale_as_expected() {
    let (bl, gains) = (64u16, [1.8f32, 1.0, 1.4]);
    let raw = [300u16, 500, 350];
    let m = mosaic(W, H, CfaPattern::Bggr, |_, _| raw);
    let (bytes, stride) = pack(&m, W, H, RawPacking::Csi2Raw10);
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let params = IspParams {
        black_level: Some(BlackLevel::uniform(bl)),
        white_balance: Some(WhiteBalance {
            r: gains[0],
            g: gains[1],
            b: gains[2],
        }),
        digital_gain: 1.1,
        ..Default::default()
    };
    let (out, _) = rgb(format, &params, &bytes, stride, Scale::Full);
    for c in 0..3 {
        let want = (raw[c] - bl) as f32 * gains[c] * 1.1 * 1023.0 / (1023.0 - bl as f32) / 4.0;
        let got = out[3 * (W * 10 + 10) + c] as f32;
        assert!((got - want).abs() <= 1.0, "channel {c}: {got} vs {want}");
    }
}

#[test]
fn ccm_and_tone_curve_apply() {
    let m = mosaic(W, H, CfaPattern::Rggb, |_, _| [512, 256, 128]);
    let (bytes, stride) = pack(&m, W, H, RawPacking::Csi2Raw10);
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
    // Swap red and blue, then gamma 2.
    let params = IspParams {
        ccm: Some(ColorMatrix {
            m: [[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]],
        }),
        tone: Some(ToneCurve::Gamma { gamma: 2.0 }),
        ..Default::default()
    };
    let (out, _) = rgb(format, &params, &bytes, stride, Scale::Full);
    let expect = |lin: f32| ((lin / 4095.0).sqrt() * 255.0).round() as u8;
    assert_eq!(out[..3], [expect(512.0), expect(1024.0), expect(2048.0)]);
}

#[test]
fn yuv_outputs_match_the_colour() {
    let c8 = [200u8, 120, 40];
    let m = mosaic(W, H, CfaPattern::Grbg, |_, _| {
        raw_colour(c8.map(|c| c as u16), 10)
    });
    let (bytes, stride) = pack(&m, W, H, RawPacking::Csi2Raw10);
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Grbg, RawPacking::Csi2Raw10);
    for (matrix, coeffs) in [
        (YuvMatrix::Bt709Limited, YuvCoeffs::BT709_LIMITED),
        (YuvMatrix::Bt601Full, YuvCoeffs::BT601_FULL),
    ] {
        let params = IspParams {
            yuv: matrix,
            ..Default::default()
        };
        let mut want_y = [0u8];
        scalar::rgb_to_y_row([&c8[0..1], &c8[1..2], &c8[2..3]], &mut want_y, 1, &coeffs);
        let (r, g, b) = (c8[0] as i32, c8[1] as i32, c8[2] as i32);
        let (want_u, want_v) = (
            scalar::chroma(&coeffs.u, r, g, b),
            scalar::chroma(&coeffs.v, r, g, b),
        );

        let (mut y, mut uv) = (vec![0u8; W * H], vec![0u8; W * H / 2]);
        process(
            format,
            &params,
            &bytes,
            stride,
            Scale::Full,
            OutputBuffers::Nv12 {
                y: &mut y,
                y_stride: W,
                uv: &mut uv,
                uv_stride: W,
            },
        )
        .unwrap();
        assert!(y.iter().all(|&v| v == want_y[0]));
        assert!(
            uv.chunks_exact(2).all(|p| p == [want_u, want_v]),
            "{matrix:?}"
        );

        let (mut y, mut u, mut v) = (vec![0u8; W * H], vec![0u8; W * H / 4], vec![0u8; W * H / 4]);
        let out = OutputBuffers::I420 {
            y: &mut y,
            y_stride: W,
            u: &mut u,
            u_stride: W / 2,
            v: &mut v,
            v_stride: W / 2,
        };
        process(format, &params, &bytes, stride, Scale::Full, out).unwrap();
        assert!(y.iter().all(|&p| p == want_y[0]));
        assert!(u.iter().all(|&p| p == want_u) && v.iter().all(|&p| p == want_v));
    }
    // Grey is neutral; white is 235 in limited range.
    let m = mosaic(W, H, CfaPattern::Grbg, |_, _| [1023; 3]);
    let (bytes, stride) = pack(&m, W, H, RawPacking::Csi2Raw10);
    let (mut y, mut uv) = (vec![0u8; W * H], vec![0u8; W * H / 2]);
    process(
        format,
        &IspParams::default(),
        &bytes,
        stride,
        Scale::Full,
        OutputBuffers::Nv12 {
            y: &mut y,
            y_stride: W,
            uv: &mut uv,
            uv_stride: W,
        },
    )
    .unwrap();
    assert!(y.iter().all(|&v| v == 235) && uv.iter().all(|&v| v == 128));
}

#[test]
fn luma_comes_straight_from_the_mosaic() {
    let c8 = [200u16, 120, 40];
    for pattern in PATTERNS {
        let m = mosaic(W, H, pattern, |_, _| c8.map(|c| c << 2));
        let (bytes, stride) = pack(&m, W, H, RawPacking::Csi2Raw10);
        let format = RawFormat::new(W as u32, H as u32, pattern, RawPacking::Csi2Raw10);
        let want = ((c8[0] + 2 * c8[1] + c8[2]) / 4) as u8;
        for scale in [Scale::Full, Scale::Half] {
            let n = if scale == Scale::Full {
                W * H
            } else {
                W * H / 4
            };
            let ow = if scale == Scale::Full { W } else { W / 2 };
            let mut y = vec![0u8; n];
            process(
                format,
                &IspParams::default(),
                &bytes,
                stride,
                scale,
                OutputBuffers::Luma {
                    data: &mut y,
                    stride: ow,
                },
            )
            .unwrap();
            assert!(y.iter().all(|&v| v == want), "{pattern:?} {scale:?}");
        }
    }
}

/// Colour error (|R - G| + |B - G|) summed over a demosaiced grey image.
fn false_colour(out: &[u8]) -> u64 {
    out.chunks_exact(3)
        .map(|p| {
            (p[0] as i32 - p[1] as i32).unsigned_abs() as u64
                + (p[2] as i32 - p[1] as i32).unsigned_abs() as u64
        })
        .sum()
}

#[test]
fn mhc_has_less_false_colour_than_bilinear_on_grey_detail() {
    // A grey zone plate: fine detail everywhere, no colour.
    let m = mosaic(W, H, CfaPattern::Bggr, |x, y| {
        let r2 = ((x * x + y * y) as f32) / 40.0;
        [(512.0 + 400.0 * r2.sin()) as u16; 3]
    });
    let (bytes, stride) = pack(&m, W, H, RawPacking::Csi2Raw10);
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let run = |demosaic| {
        rgb(
            format,
            &IspParams {
                demosaic,
                ..Default::default()
            },
            &bytes,
            stride,
            Scale::Full,
        )
        .0
    };
    let (bilinear, mhc) = (
        false_colour(&run(Demosaic::Bilinear)),
        false_colour(&run(Demosaic::Mhc)),
    );
    assert!(mhc * 4 < bilinear * 3, "MHC {mhc} vs bilinear {bilinear}");
}

#[test]
fn lens_shading_brightens_the_corners() {
    let m = mosaic(W, H, CfaPattern::Rggb, |_, _| [400; 3]);
    let (bytes, stride) = pack(&m, W, H, RawPacking::Csi2Raw10);
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
    let params = IspParams {
        lens_shading: Some(LensShading::radial(9, 7, 1.0)),
        ..Default::default()
    };
    let (out, _) = rgb(format, &params, &bytes, stride, Scale::Full);
    // The sampled channel of a pixel (red at the top-left corner, blue at the bottom-right).
    let at = |x: usize, y: usize, c: usize| out[3 * (y * W + x) + c] as f32;
    // 400 / 4 = 100 in the centre, doubled in the corners.
    assert!(
        (at(W / 2, H / 2, 1) - 100.0).abs() <= 2.0,
        "centre {}",
        at(W / 2, H / 2, 1)
    );
    assert!((at(0, 0, 0) - 200.0).abs() <= 1.0, "corner {}", at(0, 0, 0));
    assert!((at(W - 1, H - 1, 2) - 200.0).abs() <= 1.0);
}

#[test]
fn bad_inputs_are_refused() {
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
    let stride = format.min_stride();
    let raw = vec![0u8; stride * H];
    let mut out = vec![0u8; W * H * 3];
    let short = process(
        format,
        &IspParams::default(),
        &raw[..stride * H - 1],
        stride,
        Scale::Full,
        OutputBuffers::Rgb24 {
            data: &mut out,
            stride: W * 3,
        },
    );
    assert!(matches!(short, Err(IspError::InputTooShort(_))));
    let small = process(
        format,
        &IspParams::default(),
        &raw,
        stride,
        Scale::Full,
        OutputBuffers::Rgb24 {
            data: &mut out[..10],
            stride: W * 3,
        },
    );
    assert!(matches!(small, Err(IspError::OutputTooSmall(_))));
    let odd = RawFormat::new(63, 48, CfaPattern::Rggb, RawPacking::Csi2Raw10);
    assert!(matches!(
        SoftIsp::new(odd, IspParams::default()),
        Err(IspError::Unsupported(_))
    ));
    let bad = IspParams {
        black_level: Some(BlackLevel::uniform(2000)),
        ..Default::default()
    };
    assert!(matches!(
        SoftIsp::new(format, bad),
        Err(IspError::InvalidParams(_))
    ));
}

#[test]
fn params_round_trip_through_serde_json_shape() {
    // Plain data: Debug + Clone + PartialEq, and serde derives (checked by type).
    fn serde_able<T: serde::Serialize + for<'de> serde::Deserialize<'de>>() {}
    serde_able::<IspParams>();
    serde_able::<IspStats>();
    serde_able::<RawFormat>();
}

/// A real OV9782 frame (1280x800 `pBAA`, stride 1600), when present: a dark frame, so this is
/// only a smoke test. Set `STYX_SOFTISP_FRAME` to use another file.
#[test]
fn device_frame_smoke_test() {
    let path = std::env::var("STYX_SOFTISP_FRAME").unwrap_or_else(|_| {
        "/tmp/claude-1000/-run-media-sozo-bd1d96d9-fa81-4fac-b25e-193cfcac2dcb-Github-Styx/4e901f50-3dbf-41b1-aef6-680fc511fa44/scratchpad/frame.raw".into()
    });
    let Ok(raw) = std::fs::read(&path) else {
        eprintln!("no device frame at {path}; skipped");
        return;
    };
    let format = RawFormat::new(1280, 800, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let params = IspParams {
        stats: Some(StatsConfig::default()),
        tone: Some(ToneCurve::Srgb),
        ..Default::default()
    };
    let (mut y, mut uv) = (vec![0u8; 1280 * 800], vec![0u8; 1280 * 400]);
    let stats = process(
        format,
        &params,
        &raw,
        1600,
        Scale::Full,
        OutputBuffers::Nv12 {
            y: &mut y,
            y_stride: 1280,
            uv: &mut uv,
            uv_stride: 1280,
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(stats.samples, 640 * 400);
    assert_eq!(stats.histogram.iter().sum::<u32>(), 640 * 400);
    assert!(
        stats.mean_luma() < 0.2,
        "a dark frame: {}",
        stats.mean_luma()
    );
}
