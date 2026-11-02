//! The output stages (luma and chroma denoise, sharpening) against `styx-softisp`'s integer
//! arithmetic, bit for bit: each stage alone and all together, every output at both scales,
//! even and odd picture sizes, both YUV matrices, on every Vulkan device of the machine.

use crate::common::{all_outputs, contexts, pack};
use crate::equivalence::{check, frame, params};
use styx_gpuisp::GpuIsp;
use styx_softisp::*;

const LUMA: [LumaDenoise; 2] = [
    LumaDenoise {
        constant: 400.0,
        slope: 20.0,
        strength: 1.0,
    },
    LumaDenoise {
        constant: 2000.0,
        slope: 120.0,
        strength: 0.6,
    },
];

const CHROMA: [ChromaDenoise; 2] = [
    ChromaDenoise {
        threshold: 3000.0,
        strength: 1.0,
    },
    ChromaDenoise {
        threshold: 900.0,
        strength: 0.5,
    },
];

const SHARPEN: [Sharpen; 2] = [
    Sharpen {
        amount: 0.8,
        threshold: 2.0,
        limit: 20.0,
    },
    Sharpen {
        amount: 1.5,
        threshold: 0.0,
        limit: 6.0,
    },
];

/// Frames of even and odd picture sizes (the half scale of 262x98 and 38x26 is odd).
const SIZES: [(usize, usize, CfaPattern); 4] = [
    (262, 98, CfaPattern::Bggr),
    (70, 34, CfaPattern::Grbg),
    (38, 26, CfaPattern::Rggb),
    (6, 8, CfaPattern::Gbrg),
];

/// Every output of every size, demosaic and matrix, with the output stages `denoise` and
/// `sharpen` on top of the usual settings.
fn bit_exact(denoise: Option<Denoise>, sharpen: Option<Sharpen>) {
    for ctx in contexts() {
        for (w, h, pattern) in SIZES {
            let m = frame(w, h, pattern, 10);
            let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
            let format = RawFormat::new(w as u32, h as u32, pattern, RawPacking::Csi2Raw10);
            for demosaic in [Demosaic::Bilinear, Demosaic::Mhc] {
                for yuv in [YuvMatrix::Bt601Full, YuvMatrix::Bt709Limited] {
                    let mut p = params(demosaic, true, true, 64, 1);
                    if let Some(s) = &mut p.stats {
                        s.zones_x = s.zones_x.min(w as u32 / 2);
                        s.zones_y = s.zones_y.min(h as u32 / 2);
                    }
                    p.denoise = denoise;
                    p.sharpen = sharpen;
                    p.yuv = yuv;
                    check(&ctx, format, &p, &raw, stride);
                }
            }
        }
    }
}

fn luma(l: LumaDenoise) -> Denoise {
    Denoise {
        luma: Some(l),
        chroma: None,
    }
}

fn chroma(c: ChromaDenoise) -> Denoise {
    Denoise {
        luma: None,
        chroma: Some(c),
    }
}

#[test]
fn denoise_luma_bit_exact() {
    for l in LUMA {
        bit_exact(Some(luma(l)), None);
    }
}

#[test]
fn denoise_chroma_bit_exact() {
    for c in CHROMA {
        bit_exact(Some(chroma(c)), None);
    }
}

#[test]
fn denoise_both_bit_exact() {
    for (l, c) in [(LUMA[0], CHROMA[0]), (LUMA[1], CHROMA[1])] {
        bit_exact(
            Some(Denoise {
                luma: Some(l),
                chroma: Some(c),
            }),
            None,
        );
    }
}

#[test]
fn sharpen_bit_exact() {
    for s in SHARPEN {
        bit_exact(None, Some(s));
    }
}

#[test]
fn all_stages_bit_exact() {
    bit_exact(
        Some(Denoise {
            luma: Some(LUMA[0]),
            chroma: Some(CHROMA[0]),
        }),
        Some(SHARPEN[0]),
    );
    bit_exact(
        Some(Denoise {
            luma: Some(LUMA[1]),
            chroma: Some(CHROMA[1]),
        }),
        Some(SHARPEN[1]),
    );
}

/// Windows of the frame at both scales with the output stages on: the picture is made whole
/// and the window cropped from it, so the pixels are `process_window`'s of `styx-softisp`.
#[test]
fn windows_bit_exact() {
    let windows = [
        Window::new(0, 0, 264, 100),
        Window::new(22, 10, 100, 46),
        Window::new(0, 0, 64, 32),
        Window::new(200, 68, 64, 32),
        Window::new(120, 40, 2, 2),
    ];
    for ctx in contexts() {
        let (w, h) = (264, 100);
        let m = frame(w, h, CfaPattern::Rggb, 10);
        let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
        let format = RawFormat::new(w as u32, h as u32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
        let mut p = params(Demosaic::Bilinear, true, true, 64, 1);
        p.denoise = Some(Denoise {
            luma: Some(LUMA[0]),
            chroma: Some(CHROMA[0]),
        });
        p.sharpen = Some(SHARPEN[0]);
        let mut cpu = SoftIsp::new(format, p.clone()).unwrap();
        let mut gpu = GpuIsp::with_context(&ctx, format, p).unwrap();
        for win in windows {
            for scale in [Scale::Full, Scale::Half] {
                let (ow, oh) = cpu.window_size(win, scale);
                let size = (ow as usize, oh as usize);
                let (want, _) = all_outputs(size, &mut |o| {
                    cpu.process_window(&raw, stride, win, scale, o).unwrap();
                    None
                });
                let (got, _) = all_outputs(size, &mut |o| {
                    gpu.process_window(&raw, stride, win, scale, o).unwrap();
                    None
                });
                for ((name, a), (_, b)) in want.iter().zip(&got) {
                    if a != b {
                        let at = a.iter().zip(b).position(|(p, q)| p != q).unwrap_or(0);
                        panic!(
                            "{}: {name} of {win:?} differs at byte {at} ({} vs {}), {scale:?}",
                            ctx.info().name,
                            a[at],
                            b[at],
                        );
                    }
                }
            }
        }
    }
}

/// The output stages switch on and off between frames (the 3A loop's settings change), and
/// the picture stays bit-exact each frame.
#[test]
fn stages_toggled_between_frames() {
    for ctx in contexts() {
        let (w, h) = (128, 64);
        let m = frame(w, h, CfaPattern::Bggr, 10);
        let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
        let format = RawFormat::new(w as u32, h as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
        let mut gpu = GpuIsp::with_context(&ctx, format, IspParams::default()).unwrap();
        for k in 0..6 {
            let mut p = params(Demosaic::Bilinear, k % 2 == 1, true, 64, 1);
            p.denoise = (k % 3 != 0).then(|| Denoise {
                luma: Some(LUMA[k % 2]),
                chroma: (k % 2 == 0).then_some(CHROMA[0]),
            });
            p.sharpen = (k >= 2).then_some(SHARPEN[k % 2]);
            gpu.set_params(p.clone()).unwrap();
            let mut cpu = SoftIsp::new(format, p).unwrap();
            let (mut a, mut b) = (vec![0u8; w * h * 3], vec![0u8; w * h * 3]);
            cpu.process(
                &raw,
                stride,
                Scale::Full,
                OutputBuffers::Rgb24 {
                    data: &mut a,
                    stride: w * 3,
                },
            )
            .unwrap();
            gpu.process(
                &raw,
                stride,
                Scale::Full,
                OutputBuffers::Rgb24 {
                    data: &mut b,
                    stride: w * 3,
                },
            )
            .unwrap();
            assert!(
                a == b,
                "{}: frame {k} differs at byte {}",
                ctx.info().name,
                a.iter().zip(&b).position(|(p, q)| p != q).unwrap_or(0)
            );
        }
    }
}
