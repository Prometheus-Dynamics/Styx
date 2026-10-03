//! The FP16 leaves against the scalar oracle (bit for bit; on CPUs without FP16 both sides
//! are the oracle), and the oracle's arithmetic on known values.

use super::*;
use crate::simd::RowKind;

/// Deterministic random values.
struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }

    /// Working values as the front end makes them: fp16 in 0..=4080, a fifth of rows
    /// only 0 and 4080.
    fn working(&mut self, n: usize) -> Vec<u16> {
        let extreme = self.next().is_multiple_of(5);
        (0..n)
            .map(|_| {
                let v = self.next();
                if extreme {
                    if v & 1 == 0 { 0 } else { H_MAX }
                } else {
                    f16::min(
                        f16::from_f64(f64::from(v % 4096) + f64::from(v >> 28) / 16.0),
                        H_MAX,
                    )
                }
            })
            .collect()
    }
}

/// Run `f` with the leaves (when the CPU has FP16) and with the oracle alone.
fn both<T: PartialEq + std::fmt::Debug>(mut f: impl FnMut() -> T) {
    let fast = f();
    SCALAR_ONLY.with(|s| s.set(true));
    let slow = f();
    SCALAR_ONLY.with(|s| s.set(false));
    assert_eq!(fast, slow);
}

const GUARD: u16 = 0xA5A7;

const WIDTHS: [usize; 7] = [2, 8, 16, 18, 46, 64, 98];

fn tones() -> Vec<HalfTone> {
    vec![
        HalfTone::from_curve(|x| crate::ToneCurve::Srgb.eval(x)).unwrap(),
        HalfTone::from_curve(|x| x).unwrap(),
        HalfTone::from_curve(|x| (x * 3.0).min(1.0)).unwrap(),
    ]
}

#[test]
fn front_matches_the_oracle() {
    let mut rng = Rng(7);
    for w in WIDTHS {
        for seed in 0..6 {
            let raw: Vec<u16> = (0..w).map(|_| (rng.next() % 1024) as u16).collect();
            let a = rng
                .working(w)
                .iter()
                .map(|&v| f16::mul(v, 0x1000))
                .collect::<Vec<_>>();
            let d = rng
                .working(w)
                .iter()
                .map(|&v| f16::mul(v, 0x0400))
                .collect::<Vec<_>>();
            let black = [f16::from_f64(1024.0 + 64.0), f16::from_f64(1024.0 + 60.0)];
            let gain = [f16::from_f64(7.2), f16::from_f64(4.1)];
            let t = f16::from_f64(f64::from(seed) / 6.0);
            both(|| {
                let mut out = raw.clone();
                out.push(GUARD);
                let lsc = (seed % 2 == 1).then_some(LscRow { a: &a, d: &d, t });
                front_row(&mut out, black, gain, lsc, w);
                assert_eq!(out[w], GUARD);
                out
            });
        }
    }
    // 1023 above a black level of 64, gain 4080 / 959: full scale.
    let mut out = [1023, 10];
    let g = f16::from_f64(4080.0 / 959.0);
    front_row(&mut out, [f16::from_f64(1088.0); 2], [g; 2], None, 2);
    assert_eq!(f16::to_f64(out[0]), 4080.0);
    assert_eq!(out[1], 0);
}

#[test]
fn packed_front_matches_unpack_then_front() {
    let mut rng = Rng(9);
    for w in [4, 8, 12, 16, 36, 64, 100] {
        let bytes: Vec<u8> = (0..crate::simd::raw10_bytes(w) + 7)
            .map(|_| rng.next() as u8)
            .collect();
        let a = rng.working(w);
        let d: Vec<u16> = rng
            .working(w)
            .iter()
            .map(|&v| f16::mul(v, 0x0400))
            .collect();
        let black = [f16::from_f64(1088.0), f16::from_f64(1090.0)];
        let gain = [f16::from_f64(5.1), f16::from_f64(3.3)];
        for lsc in [
            None,
            Some(LscRow {
                a: &a,
                d: &d,
                t: 0x3555,
            }),
        ] {
            let mut want = vec![0u16; w];
            crate::simd::scalar::unpack_raw10_row(&bytes, &mut want, w);
            SCALAR_ONLY.with(|s| s.set(true));
            front_row(&mut want, black, gain, lsc, w);
            SCALAR_ONLY.with(|s| s.set(false));
            both(|| {
                let mut got = vec![GUARD; w + 1];
                front_raw10_row(&bytes, &mut got, black, gain, lsc, w);
                assert_eq!(got[w], GUARD);
                assert_eq!(got[..w], want[..], "width {w}");
                got
            });
        }
    }
}

#[test]
fn colour_matches_the_oracle() {
    let mut rng = Rng(11);
    let m = [[1.6, -0.4, -0.2], [-0.3, 1.5, -0.2], [-0.1, -0.5, 1.6]];
    for w in WIDTHS {
        for (n, tone) in tones().iter().enumerate() {
            for pattern in [CfaPattern::Bggr, CfaPattern::Grbg] {
                for y in 0..2 {
                    let cc = ColourCoeffs::new(&m, RowKind::of(pattern, y));
                    let rows: Vec<Vec<u16>> = (0..3).map(|_| rng.working(w + 2)).collect();
                    let rows = [&rows[0][..], &rows[1][..], &rows[2][..]];
                    both(|| {
                        let mut planes = vec![vec![0u8; w + 1]; 3];
                        let [r, g, b] = &mut planes[..] else {
                            unreachable!()
                        };
                        colour_row(rows, ColourOut::Planes([r, g, b]), w, &cc, tone);
                        let mut packed = vec![0u8; 3 * w + 1];
                        colour_row(rows, ColourOut::Packed(&mut packed), w, &cc, tone);
                        let mut again = vec![vec![0u8; w + 1]; 3];
                        let mut y = vec![0u8; w + 1];
                        let [r, g, b] = &mut again[..] else {
                            unreachable!()
                        };
                        let c = crate::simd::YuvCoeffs::BT601_FULL;
                        colour_row(
                            rows,
                            ColourOut::PlanesLuma([r, g, b], &mut y, &c),
                            w,
                            &cc,
                            tone,
                        );
                        assert_eq!(again, planes);
                        let mut want = vec![0u8; w];
                        let p = [&planes[0][..], &planes[1][..], &planes[2][..]];
                        crate::simd::scalar::rgb_to_y_row(p, &mut want, w, &c);
                        assert_eq!(y[..w], want[..]);
                        // The second row of a pair: its luma and the pair's chroma, from the
                        // first row's planes (here: the packed row's, de-interleaved).
                        let top: Vec<Vec<u8>> = (0..3)
                            .map(|k| packed.iter().skip(k).step_by(3).copied().collect())
                            .collect();
                        let (mut y2, mut uv) = (vec![0u8; w + 1], vec![0u8; w + 1]);
                        let mut scratch = vec![vec![0u8; w + 1]; 3];
                        let [r, g, b] = &mut scratch[..] else {
                            unreachable!()
                        };
                        let chroma = Chroma {
                            top: [&top[0], &top[1], &top[2]],
                            u: &mut uv,
                            v: None,
                        };
                        let out = ColourOut::LumaChroma([r, g, b], &mut y2, chroma, &c);
                        colour_row(rows, out, w, &cc, tone);
                        assert_eq!(y2[..w], want[..]);
                        let mut want_uv = vec![0u8; w];
                        let t = [&top[0][..], &top[1][..], &top[2][..]];
                        crate::simd::scalar::rgb_to_uv_row(
                            t,
                            p,
                            &mut want_uv,
                            &mut [],
                            w / 2,
                            &c,
                            true,
                        );
                        assert_eq!(uv[..w], want_uv[..]);
                        for x in 0..w {
                            assert_eq!(
                                [planes[0][x], planes[1][x], planes[2][x]],
                                packed[3 * x..3 * x + 3],
                                "tone {n} x {x}"
                            );
                        }
                        (planes, packed, y, uv)
                    });
                }
            }
        }
    }
}

#[test]
fn quads_and_luma_match_the_oracle() {
    let mut rng = Rng(5);
    let m: [u16; 9] = [1.6, -0.4, -0.2, -0.3, 1.5, -0.2, -0.1, -0.5, 1.6].map(f16::from_f64);
    let tone = &tones()[0];
    for w in WIDTHS {
        for pattern in [
            CfaPattern::Rggb,
            CfaPattern::Bggr,
            CfaPattern::Grbg,
            CfaPattern::Gbrg,
        ] {
            let (top, bottom) = (rng.working(2 * w), rng.working(2 * w));
            both(|| {
                let mut planes = vec![vec![0u8; w + 1]; 3];
                let [r, g, b] = &mut planes[..] else {
                    unreachable!()
                };
                quad_colour_row(
                    &top,
                    &bottom,
                    ColourOut::Planes([r, g, b]),
                    w,
                    &m,
                    pattern,
                    tone,
                );
                let mut stats = vec![vec![0u16; w + 1]; 3];
                let [r, g, b] = &mut stats[..] else {
                    unreachable!()
                };
                quad_stats_row(&top, &bottom, [r, g, b], w, pattern, H_TO_12BIT);
                let mut luma = vec![0u8; w + 1];
                quad_luma_row(&top, &bottom, &mut luma, w, tone);
                (planes, stats, luma)
            });
        }
        let rows: Vec<Vec<u16>> = (0..3).map(|_| rng.working(w + 2)).collect();
        both(|| {
            let mut luma = vec![0u8; w + 1];
            luma_row([&rows[0], &rows[1], &rows[2]], &mut luma, w, tone);
            luma
        });
    }
}

#[test]
fn tone_follows_the_curve() {
    let srgb = |x: f32| crate::ToneCurve::Srgb.eval(x);
    let tone = HalfTone::from_curve(srgb).unwrap();
    for v in 0..4081 {
        let want = srgb(v as f32 / 4080.0) * 255.0;
        let got = f32::from(tone.apply(f16::from_f64(f64::from(v) + 16.0)));
        assert!((got - want).abs() <= 1.0, "{v}: {got} vs {want}");
    }
    assert!(HalfTone::from_curve(|x| 1.0 - x).is_none());
    // White is the curve's end.
    assert_eq!(tone.apply(f16::add(H_MAX, H_16)), 255);
    assert_eq!(f16::from_f64(4095.0 / 4080.0), H_TO_12BIT);
    assert_eq!(f16::from_f64(FULL), H_MAX);
}
