//! Windows and binned overviews: a window's pixels are the same window of the whole frame's
//! picture bit for bit, in every arithmetic, packing, output and demosaic, at the frame's
//! edges too; a binned pass gathers the whole frame's statistics.

mod common;

use common::{mosaic, pack};
use styx_softisp::*;

const W: usize = 136;
const H: usize = 72;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Rgb24,
    Luma,
    Nv12,
    I420,
}

const KINDS: [Kind; 4] = [Kind::Rgb24, Kind::Luma, Kind::Nv12, Kind::I420];

/// Planes of a `w`x`h` image of `kind`: (bytes per row, rows).
fn planes(kind: Kind, w: usize, h: usize) -> Vec<(usize, usize)> {
    match kind {
        Kind::Rgb24 => vec![(3 * w, h)],
        Kind::Luma => vec![(w, h)],
        Kind::Nv12 => vec![(w, h), (w, h / 2)],
        Kind::I420 => vec![(w, h), (w / 2, h / 2), (w / 2, h / 2)],
    }
}

/// Runs `f` on buffers for a `w`x`h` image of `kind`; returns its planes.
fn image<T>(
    kind: Kind,
    w: usize,
    h: usize,
    f: impl FnOnce(OutputBuffers<'_>) -> T,
) -> (Vec<Vec<u8>>, T) {
    let mut p: Vec<Vec<u8>> = planes(kind, w, h)
        .iter()
        .map(|&(r, n)| vec![0xAA; r * n])
        .collect();
    let out = match (kind, p.as_mut_slice()) {
        (Kind::Rgb24, [d]) => OutputBuffers::Rgb24 {
            data: d,
            stride: 3 * w,
        },
        (Kind::Luma, [d]) => OutputBuffers::Luma { data: d, stride: w },
        (Kind::Nv12, [y, uv]) => OutputBuffers::Nv12 {
            y,
            y_stride: w,
            uv,
            uv_stride: w,
        },
        (Kind::I420, [y, u, v]) => OutputBuffers::I420 {
            y,
            y_stride: w,
            u,
            u_stride: w / 2,
            v,
            v_stride: w / 2,
        },
        _ => unreachable!(),
    };
    let r = f(out);
    (p, r)
}

/// The planes of `full` (a `fw`-wide image of `kind`) cropped to `w`x`h` at (`x`, `y`).
fn crop(
    kind: Kind,
    full: &[Vec<u8>],
    fw: usize,
    (x, y, w, h): (usize, usize, usize, usize),
) -> Vec<Vec<u8>> {
    let geo = planes(kind, fw, 2);
    full.iter()
        .enumerate()
        .map(|(i, plane)| {
            let row = geo[i].0;
            // Horizontal bytes per pixel and vertical subsampling of this plane.
            let (bpp, sub) = match (kind, i) {
                (Kind::Rgb24, _) => (3.0, 1),
                (Kind::Luma, _) | (_, 0) => (1.0, 1),
                (Kind::Nv12, _) => (1.0, 2),
                (Kind::I420, _) => (0.5, 2),
            };
            let (x0, bw) = ((x as f64 * bpp) as usize, (w as f64 * bpp) as usize);
            (y / sub..(y + h) / sub)
                .flat_map(|r| plane[r * row + x0..][..bw].to_vec())
                .collect()
        })
        .collect()
}

fn frame(pattern: CfaPattern, packing: RawPacking) -> (Vec<u8>, usize) {
    let m = mosaic(W, H, pattern, |x, y| {
        let mut s = (x as u32 * 7919 + y as u32 * 104_729) ^ 0x2545_F491;
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        let noise = (s & 63) as i32 - 32;
        [
            (x * 1023 / W) as i32 + noise,
            ((x * 3 + y * 5) % 700) as i32 + noise,
            if (x / 5 + y / 7) % 2 == 0 { 900 } else { 80 } + noise,
        ]
        .map(|v| v.clamp(0, 1023) as u16)
    });
    pack(&m, W, H, packing)
}

fn params(arithmetic: Arithmetic, demosaic: Demosaic, row_step: u32) -> IspParams {
    IspParams {
        black_level: Some(BlackLevel::uniform(64)),
        white_balance: Some(WhiteBalance {
            r: 1.8,
            g: 1.0,
            b: 1.5,
        }),
        digital_gain: 1.2,
        lens_shading: Some(LensShading::radial(9, 7, 0.7)),
        demosaic,
        ccm: Some(ColorMatrix {
            m: [[1.6, -0.4, -0.2], [-0.3, 1.5, -0.2], [-0.1, -0.5, 1.6]],
        }),
        tone: Some(ToneCurve::Srgb),
        yuv: YuvMatrix::Bt601Full,
        stats: Some(StatsConfig {
            zones_x: 8,
            zones_y: 6,
            histogram_bins: 64,
            saturation: 0.95,
            row_step,
            focus: true,
            ..StatsConfig::default()
        }),
        arithmetic,
    }
}

/// Windows at the corners, edges and inside, of several sizes.
const WINDOWS: [(u32, u32, u32, u32); 7] = [
    (0, 0, 16, 16),
    (6, 10, 40, 20),
    (2, 2, 4, 4),
    (W as u32 - 24, H as u32 - 12, 24, 12),
    (W as u32 - 20, 0, 20, 36),
    (0, H as u32 - 8, W as u32, 8),
    (0, 0, W as u32, H as u32),
];

#[test]
fn windows_are_the_frame_cropped_bit_for_bit() {
    let mut checked = 0;
    for arithmetic in [Arithmetic::Int, Arithmetic::Half, Arithmetic::IntPolyTone] {
        for demosaic in [Demosaic::Bilinear, Demosaic::Mhc] {
            for (pattern, packing) in [
                (CfaPattern::Bggr, RawPacking::Csi2Raw10),
                (CfaPattern::Grbg, RawPacking::Csi2Raw12),
                (CfaPattern::Gbrg, RawPacking::U16Le { bits: 10 }),
                (CfaPattern::Rggb, RawPacking::U8),
            ] {
                let (raw, stride) = frame(pattern, packing);
                let format = RawFormat::new(W as u32, H as u32, pattern, packing);
                // Staged input on one thread, read in place on three.
                for (threads, copy) in [(1, true), (3, false)] {
                    let mut isp = SoftIsp::new(format, params(arithmetic, demosaic, 1))
                        .unwrap()
                        .with_threads(threads)
                        .with_copy_input(copy);
                    for scale in [Scale::Full, Scale::Half] {
                        let (fw, fh) = isp.output_size(scale);
                        for kind in KINDS {
                            let (full, _) = image(kind, fw as usize, fh as usize, |o| {
                                isp.process(&raw, stride, scale, o).unwrap()
                            });
                            for (x, y, w, h) in WINDOWS {
                                let win = Window::new(x, y, w, h);
                                let (ow, oh) = isp.window_size(win, scale);
                                let (ow, oh) = (ow as usize, oh as usize);
                                if matches!(kind, Kind::Nv12 | Kind::I420)
                                    && (ow % 2 != 0 || oh % 2 != 0)
                                {
                                    continue;
                                }
                                let (got, ()) = image(kind, ow, oh, |o| {
                                    isp.process_window(&raw, stride, win, scale, o).unwrap()
                                });
                                let d = if scale == Scale::Full { 1 } else { 2 };
                                let rect = (x as usize / d, y as usize / d, ow, oh);
                                let mut want = crop(kind, &full, fw as usize, rect);
                                let mut got = got;
                                // 4:2:0 chroma pairs rows and columns from the window's origin:
                                // the frame's own pairs only from an even output origin.
                                if (rect.0 | rect.1) % 2 != 0 {
                                    want.truncate(1);
                                    got.truncate(1);
                                }
                                assert!(
                                    got == want,
                                    "{arithmetic:?} {demosaic:?} {pattern:?} {packing:?} \
                                     {threads} threads {scale:?} {kind:?} {win:?}"
                                );
                                checked += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(checked > 1000, "{checked}");
}

#[test]
fn windows_are_checked() {
    let (raw, stride) = frame(CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let mut isp = SoftIsp::new(format, IspParams::default()).unwrap();
    let mut buf = vec![0u8; W * H * 3];
    for (x, y, w, h) in [
        (1, 0, 16, 16),
        (0, 0, 15, 16),
        (0, 0, 0, 16),
        (W as u32 - 14, 0, 16, 16),
        (0, H as u32, 16, 2),
    ] {
        let out = OutputBuffers::Rgb24 {
            data: &mut buf,
            stride: W * 3,
        };
        assert!(matches!(
            isp.process_window(&raw, stride, Window::new(x, y, w, h), Scale::Full, out),
            Err(IspError::Unsupported(_))
        ));
    }
    let small = OutputBuffers::Rgb24 {
        data: &mut buf[..10],
        stride: 48,
    };
    assert!(matches!(
        isp.process_window(&raw, stride, Window::new(0, 0, 16, 16), Scale::Full, small),
        Err(IspError::OutputTooSmall(_))
    ));
}

#[test]
fn binned_passes_gather_the_whole_frames_statistics() {
    for arithmetic in [Arithmetic::Int, Arithmetic::Half] {
        for row_step in [1, 2, 4] {
            let (raw, stride) = frame(CfaPattern::Grbg, RawPacking::Csi2Raw10);
            let format =
                RawFormat::new(W as u32, H as u32, CfaPattern::Grbg, RawPacking::Csi2Raw10);
            let mut isp =
                SoftIsp::new(format, params(arithmetic, Demosaic::Bilinear, row_step)).unwrap();
            let (half, want) = image(Kind::Rgb24, W / 2, H / 2, |o| {
                isp.process(&raw, stride, Scale::Half, o).unwrap()
            });
            let (full, full_stats) = image(Kind::Luma, W, H, |o| {
                isp.process(&raw, stride, Scale::Full, o).unwrap()
            });
            drop(full);
            assert_eq!(want, full_stats);
            assert!(want.is_some());
            assert_eq!(isp.statistics(&raw, stride).unwrap(), want, "{row_step}");
            // 2: the half-size picture itself.
            let (two, stats) = image(Kind::Rgb24, W / 2, H / 2, |o| {
                isp.process_binned(&raw, stride, 2, o).unwrap()
            });
            assert_eq!((&two, &stats), (&half, &want));
            // 4, 6: every second (third) quad of every second (third) quad row of it, and the
            // same statistics; 6 leaves quad rows below the last output row.
            for factor in [4u32, 6] {
                let step = factor as usize / 2;
                let (bw, bh) = isp.binned_size(factor);
                let (bw, bh) = (bw as usize, bh as usize);
                let (binned, stats) = image(Kind::Rgb24, bw, bh, |o| {
                    isp.process_binned(&raw, stride, factor, o).unwrap()
                });
                assert_eq!(stats, want, "{arithmetic:?} {row_step} {factor}");
                for y in 0..bh {
                    for x in 0..bw {
                        let h = &half[0][(y * step * (W / 2) + x * step) * 3..][..3];
                        assert_eq!(&binned[0][(y * bw + x) * 3..][..3], h, "{factor} {x} {y}");
                    }
                }
            }
        }
    }
}

#[test]
fn statistics_can_be_off() {
    let (raw, stride) = frame(CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let format = RawFormat::new(W as u32, H as u32, CfaPattern::Bggr, RawPacking::Csi2Raw10);
    let mut isp = SoftIsp::new(format, params(Arithmetic::Auto, Demosaic::Bilinear, 4)).unwrap();
    isp.set_statistics(false);
    assert_eq!(isp.statistics(&raw, stride).unwrap(), None);
    let (_, s) = image(Kind::Luma, W / 4, H / 4, |o| {
        isp.process_binned(&raw, stride, 4, o).unwrap()
    });
    assert_eq!(s, None);
    assert!(
        isp.process_binned(
            &raw,
            stride,
            3,
            OutputBuffers::Luma {
                data: &mut [],
                stride: 0
            }
        )
        .is_err()
    );
}
