//! The edge-directed demosaic against bilinear on a synthetic edge, and the output stages
//! (denoise, sharpening) on windows, threads and every output: a window's pixels are the frame's,
//! bit for bit.

use crate::common::{mosaic, pack};
use styx_softisp::*;

const W: usize = 96;
const H: usize = 64;

/// The kinds of output the stages are checked on.
const KINDS: [&str; 4] = ["rgb", "luma", "nv12", "i420"];

/// The picture of `isp` (the whole frame, or `win` of it) at `scale` as bytes: RGB24, luma, NV12
/// (Y then UV) or I420 (Y, U, V), tightly packed.
fn picture(
    isp: &mut SoftIsp,
    raw: &[u8],
    stride: usize,
    win: Option<Window>,
    scale: Scale,
    kind: &str,
) -> Vec<u8> {
    let (w, h) = match win {
        Some(win) => isp.window_size(win, scale),
        None => isp.output_size(scale),
    };
    let (w, h) = (w as usize, h as usize);
    let run = |isp: &mut SoftIsp, out: OutputBuffers<'_>| match win {
        Some(win) => isp.process_window(raw, stride, win, scale, out).map(drop),
        None => isp.process(raw, stride, scale, out).map(drop),
    };
    match kind {
        "rgb" => {
            let mut d = vec![0u8; 3 * w * h];
            run(
                isp,
                OutputBuffers::Rgb24 {
                    data: &mut d,
                    stride: 3 * w,
                },
            )
            .unwrap();
            d
        }
        "luma" => {
            let mut d = vec![0u8; w * h];
            run(
                isp,
                OutputBuffers::Luma {
                    data: &mut d,
                    stride: w,
                },
            )
            .unwrap();
            d
        }
        "nv12" => {
            let mut y = vec![0u8; w * h];
            let mut uv = vec![0u8; w * h / 2];
            run(
                isp,
                OutputBuffers::Nv12 {
                    y: &mut y,
                    y_stride: w,
                    uv: &mut uv,
                    uv_stride: w,
                },
            )
            .unwrap();
            [y, uv].concat()
        }
        _ => {
            let mut y = vec![0u8; w * h];
            let mut u = vec![0u8; w * h / 4];
            let mut v = vec![0u8; w * h / 4];
            run(
                isp,
                OutputBuffers::I420 {
                    y: &mut y,
                    y_stride: w,
                    u: &mut u,
                    u_stride: w / 2,
                    v: &mut v,
                    v_stride: w / 2,
                },
            )
            .unwrap();
            [y, u, v].concat()
        }
    }
}

/// `cw` x `ch` at `(x0, y0)` of a `pw`-wide plane, tightly packed.
fn crop(plane: &[u8], pw: usize, cw: usize, ch: usize, x0: usize, y0: usize) -> Vec<u8> {
    (0..ch)
        .flat_map(|r| plane[(y0 + r) * pw + x0..][..cw].iter().copied())
        .collect()
}

/// The part of the whole frame's picture (`whole`, from [`picture`]) that window `win` makes.
fn cropped(whole: &[u8], fw: usize, fh: usize, win: Window, scale: Scale, kind: &str) -> Vec<u8> {
    let div = if scale == Scale::Half { 2 } else { 1 };
    let (x0, y0) = (win.x as usize / div, win.y as usize / div);
    let (cw, ch) = (win.width as usize / div, win.height as usize / div);
    let (ux0, uy0, ucw, uch) = (x0 / 2, y0 / 2, cw / 2, ch / 2);
    match kind {
        "rgb" => crop(whole, 3 * fw, 3 * cw, ch, 3 * x0, y0),
        "luma" => crop(whole, fw, cw, ch, x0, y0),
        "nv12" => {
            let mut v = crop(whole, fw, cw, ch, x0, y0);
            v.extend(crop(&whole[fw * fh..], fw, 2 * ucw, uch, 2 * ux0, uy0));
            v
        }
        _ => {
            let (u, vp) = whole[fw * fh..].split_at(fw * fh / 4);
            let mut v = crop(whole, fw, cw, ch, x0, y0);
            v.extend(crop(u, fw / 2, ucw, uch, ux0, uy0));
            v.extend(crop(vp, fw / 2, ucw, uch, ux0, uy0));
            v
        }
    }
}

/// Output stages on: denoise (both parts) and sharpening, at settings that change the picture.
fn staged() -> IspParams {
    IspParams {
        denoise: Some(Denoise {
            luma: Some(LumaDenoise {
                constant: 30.0,
                slope: 400.0,
                strength: 0.7,
            }),
            chroma: Some(ChromaDenoise {
                threshold: 900.0,
                strength: 0.6,
            }),
        }),
        sharpen: Some(Sharpen {
            amount: 0.6,
            threshold: 2.0,
            limit: 10.0,
        }),
        ..Default::default()
    }
}

/// A noisy textured 10-bit mosaic.
fn noisy(pattern: CfaPattern) -> (Vec<u8>, usize) {
    let m = mosaic(W, H, pattern, |x, y| {
        let mut s =
            (x as u32).wrapping_mul(2_654_435_761) ^ (y as u32).wrapping_mul(40_503) ^ 0x9E37_79B9;
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        let base = 300 + (x * 9 + y * 5) as u32 % 500;
        let n = s % 41;
        [
            (base + n) as u16,
            (base / 2 + 2 * n) as u16,
            (base + 60 - n) as u16,
        ]
    });
    pack(&m, W, H, RawPacking::Csi2Raw10)
}

/// Mean squared error of the output (8-bit, RGB24) of `demosaic` against `truth` (10-bit
/// samples) over the frame's interior.
fn demosaic_mse(
    raw: &[u8],
    stride: usize,
    format: RawFormat,
    demosaic: Demosaic,
    truth: impl Fn(usize, usize) -> u16,
) -> f64 {
    let params = IspParams {
        demosaic,
        ..Default::default()
    };
    let mut isp = SoftIsp::new(format, params).unwrap();
    let mut out = vec![0u8; 3 * W * H];
    isp.process(
        raw,
        stride,
        Scale::Full,
        OutputBuffers::Rgb24 {
            data: &mut out,
            stride: 3 * W,
        },
    )
    .unwrap();
    let (mut sum, mut n) = (0.0f64, 0.0f64);
    for y in 3..H - 3 {
        for x in 3..W - 3 {
            for k in 0..3 {
                // 10-bit samples land on 8-bit output at a quarter.
                let d = out[3 * (y * W + x) + k] as f64 - truth(x, y) as f64 / 4.0;
                sum += d * d;
                n += 1.0;
            }
        }
    }
    sum / n
}

#[test]
fn edge_directed_beats_bilinear_on_luminance_edges() {
    // Grey steps (every channel the same, so the colour difference is zero and only the
    // green's direction matters): a vertical, a horizontal and a slanted edge, and a zipper of
    // 2x2 blocks. The edge-directed demosaic follows the edge; bilinear mixes both sides.
    let (lo, hi) = (600u16, 200u16);
    type Luma = Box<dyn Fn(usize, usize) -> u16>;
    let patterns: [(&str, Luma); 4] = [
        (
            "vertical",
            Box::new(move |x, _| if x < W / 2 { lo } else { hi }),
        ),
        (
            "horizontal",
            Box::new(move |_, y| if y < H / 2 { lo } else { hi }),
        ),
        (
            "slanted",
            Box::new(move |x, y| if 3 * x + 2 * y < 2 * W + 60 { lo } else { hi }),
        ),
        (
            "zipper",
            Box::new(move |x, y| if (x / 2 + y / 2) % 2 == 0 { lo } else { hi }),
        ),
    ];
    for (name, luma) in patterns {
        let truth = |x: usize, y: usize| luma(x, y);
        let m = mosaic(W, H, CfaPattern::Rggb, |x, y| [truth(x, y); 3]);
        let (raw, stride) = pack(&m, W, H, RawPacking::Csi2Raw10);
        let format = RawFormat::new(W as u32, H as u32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
        let bilinear = demosaic_mse(&raw, stride, format, Demosaic::Bilinear, truth);
        let edge = demosaic_mse(&raw, stride, format, Demosaic::EdgeDirected, truth);
        eprintln!("{name}: MSE bilinear {bilinear:.2}, edge-directed {edge:.2}");
        assert!(
            edge < bilinear,
            "{name}: edge-directed {edge} vs bilinear {bilinear}"
        );
    }
}

#[test]
fn stages_on_a_window_are_the_frame_cropped_bit_for_bit() {
    let windows = [
        Window::new(0, 0, 32, 16),
        Window::new(10, 6, 40, 30),
        Window::new(64, 40, 32, 24),
        Window::new(0, 0, W as u32, H as u32),
    ];
    for pattern in [CfaPattern::Bggr, CfaPattern::Grbg] {
        let (raw, stride) = noisy(pattern);
        let format = RawFormat::new(W as u32, H as u32, pattern, RawPacking::Csi2Raw10);
        for demosaic in [Demosaic::Bilinear, Demosaic::EdgeDirected] {
            for threads in [1, 3] {
                let params = IspParams {
                    demosaic,
                    ..staged()
                };
                let mut isp = SoftIsp::new(format, params).unwrap().with_threads(threads);
                for scale in [Scale::Full, Scale::Half] {
                    for kind in KINDS {
                        let whole = picture(&mut isp, &raw, stride, None, scale, kind);
                        let (fw, fh) = isp.output_size(scale);
                        for win in windows {
                            // 4:2:0 needs even output sides.
                            let (ww, wh) = isp.window_size(win, scale);
                            if matches!(kind, "nv12" | "i420") && (ww % 2 != 0 || wh % 2 != 0) {
                                continue;
                            }
                            let want = cropped(&whole, fw as usize, fh as usize, win, scale, kind);
                            let got = picture(&mut isp, &raw, stride, Some(win), scale, kind);
                            assert!(
                                got == want,
                                "{pattern:?} {demosaic:?} threads {threads} {scale:?} {kind} {win:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn stages_on_threads_match_one_thread() {
    let (raw, stride) = noisy(CfaPattern::Rggb);
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
    let mut one = SoftIsp::new(format, staged()).unwrap();
    let want = picture(&mut one, &raw, stride, None, Scale::Full, "rgb");
    for threads in [0, 2, 5] {
        let mut many = SoftIsp::new(format, staged())
            .unwrap()
            .with_threads(threads);
        let got = picture(&mut many, &raw, stride, None, Scale::Full, "rgb");
        assert!(got == want, "threads {threads}");
    }
}

#[test]
fn zero_strength_stages_stay_within_a_code_of_off() {
    // The stages work in JFIF YCbCr: with nothing to change, the picture passes the colour
    // round trip (a code or so), and nothing else.
    let (raw, stride) = noisy(CfaPattern::Bggr);
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let mut off = SoftIsp::new(format, IspParams::default()).unwrap();
    let want = picture(&mut off, &raw, stride, None, Scale::Full, "rgb");
    let zero = IspParams {
        denoise: Some(Denoise {
            luma: Some(LumaDenoise {
                constant: 0.0,
                slope: 0.0,
                strength: 0.0,
            }),
            chroma: Some(ChromaDenoise {
                threshold: 0.0,
                strength: 0.0,
            }),
        }),
        sharpen: Some(Sharpen {
            amount: 0.0,
            threshold: 0.0,
            limit: 0.0,
        }),
        ..Default::default()
    };
    let mut on = SoftIsp::new(format, zero).unwrap();
    let got = picture(&mut on, &raw, stride, None, Scale::Full, "rgb");
    let worst = want
        .iter()
        .zip(&got)
        .map(|(a, b)| (*a as i32 - *b as i32).abs())
        .max()
        .unwrap();
    assert!(worst <= 2, "worst difference {worst}");
}

#[test]
fn binned_overview_runs_with_the_stages_on() {
    // The overview is the whole frame binned: the stages work on that picture.
    let (raw, stride) = noisy(CfaPattern::Rggb);
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
    let params = IspParams {
        stats: Some(StatsConfig::default()),
        ..staged()
    };
    let mut isp = SoftIsp::new(format, params).unwrap();
    let (bw, bh) = isp.binned_size(4);
    let mut out = vec![0u8; 3 * bw as usize * bh as usize];
    let stats = isp
        .process_binned(
            &raw,
            stride,
            4,
            OutputBuffers::Rgb24 {
                data: &mut out,
                stride: 3 * bw as usize,
            },
        )
        .unwrap();
    assert!(stats.is_some());
    assert!(out.iter().any(|&v| v != 0));
}
