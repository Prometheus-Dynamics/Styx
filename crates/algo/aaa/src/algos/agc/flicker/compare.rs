//! Test-only: the fit against its reference (the fit before the trigonometry cache, see
//! [`super::reference`]) on long randomized sequences, and the cost of `add` for both.

use core::f64::consts::PI;
use std::time::Instant;

use super::reference::FlickerFit as Reference;
use super::{FlickerFit, HARMONICS};

/// A small deterministic generator (xorshift).
struct Rng(u64);

impl Rng {
    /// Uniform in `[0, 1)`.
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.next()
    }

    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[(self.next() * xs.len() as f64) as usize % xs.len()]
    }
}

/// Light flickering at `parts` `(hz, depth)`, a frame starting at `t0` exposed for `t` (as the
/// module's own tests model it).
fn light(parts: &[(f64, f64)], t0: f64, t: f64) -> f64 {
    1.0 + parts
        .iter()
        .map(|&(hz, depth)| {
            let w = 2.0 * PI * hz;
            depth * ((w * (t0 + t)).sin() - (w * t0).sin()) / (w * t)
        })
        .sum::<f64>()
}

/// One frame of a sequence: (start time, frame duration, exposure, level).
type Frame = (f64, f64, f64, f64);

/// A randomized sequence: frame rate, exposure, flicker, noise, and scene changes.
struct Seq {
    rng: Rng,
    fps: f64,
    exposure: f64,
    parts: Vec<(f64, f64)>,
    base: f64,
    noise: f64,
    frame: usize,
    /// Frames until the next scene change (0: never).
    change_in: usize,
    /// Frames at a fixed exposure in a row; the exposure is redrawn after.
    hold: usize,
}

impl Seq {
    fn new(seed: u64, mains: f64) -> Self {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let fps = rng.pick(&[30.0, 60.0, 90.0, 120.0]);
        let mut s = Self {
            rng,
            fps,
            exposure: 0.5 / fps,
            parts: Vec::new(),
            base: 0.3,
            noise: 0.0,
            frame: 0,
            change_in: 0,
            hold: 0,
        };
        s.redraw(mains, 1.0 / fps);
        s
    }

    /// Draws the lamp, the exposure and the noise for a new run of frames.
    fn redraw(&mut self, mains: f64, fd: f64) {
        let rng = &mut self.rng;
        self.parts.clear();
        let n = (rng.next() * 4.0) as usize;
        let offset = rng.range(-0.3, 0.3);
        for _ in 0..n {
            let k = rng.pick(&[1.0, 2.0, 3.0, 2.0]);
            let hz = if rng.next() < 0.8 {
                k * (mains + offset)
            } else {
                rng.range(20.0, 200.0)
            };
            self.parts.push((hz, rng.range(0.0, 0.4)));
        }
        let rng = &mut self.rng;
        self.exposure = match (rng.next() * 3.0) as usize {
            0 => rng.range(0.1, 0.9) * fd,
            1 => fd * rng.pick(&[0.5, 1.0, 2.0, 0.25]),
            _ => rng.range(0.001, 0.9 * fd),
        };
        self.noise = rng.range(0.0, 0.01);
        self.hold = 1 + (self.rng.next() * 200.0) as usize;
    }

    fn next_frame(&mut self, mains: f64) -> Frame {
        let fd = 1.0 / self.fps;
        if self.hold == 0 {
            self.redraw(mains, fd);
        }
        self.hold = self.hold.saturating_sub(1);
        if self.change_in == 0 {
            self.change_in = (self.rng.next() * 400.0) as usize;
            if self.rng.next() < 0.5 {
                self.base = self.rng.range(0.1, 0.8);
            }
        } else {
            self.change_in -= 1;
        }
        let time = self.frame as f64 * fd;
        self.frame += 1;
        let t = self.exposure;
        let t0 = time + fd - t;
        let noise = 1.0 + self.noise * (self.rng.next() - 0.5);
        (time, fd, t, self.base * light(&self.parts, t0, t) * noise)
    }
}

/// Largest differences seen between the two fits, for the report.
#[derive(Default)]
struct Worst {
    hz: f64,
    level: f64,
    f_stat: f64,
    visible: f64,
    rss: f64,
    terms: f64,
    correction: f64,
}

/// Compares the two fits after a frame, adding the largest differences to `worst`.
fn compare(new: &FlickerFit, old: &Reference, probe: Frame, worst: &mut Worst, at: &str) {
    assert!(
        (new.hz() - old.hz()).abs() < 1e-9,
        "{at}: hz {} vs {}",
        new.hz(),
        old.hz()
    );
    worst.hz = worst.hz.max((new.hz() - old.hz()).abs());
    assert_eq!(new.avoid_period(), old.avoid_period(), "{at}: avoid_period");
    match (new.model(), old.model()) {
        (None, None) => {}
        (Some(a), Some(b)) => {
            assert_eq!(
                a.significant, b.significant,
                "{at}: significance {a:?} vs {b:?}"
            );
            assert_eq!(a.samples, b.samples, "{at}: samples");
            assert_eq!(
                a.terms.len(),
                b.terms.len(),
                "{at}: terms {:?} vs {:?}",
                a.terms,
                b.terms
            );
            let rel = |x: f64, y: f64| (x - y).abs() / y.abs().max(1e-300);
            let terms = a
                .terms
                .iter()
                .zip(&b.terms)
                .map(|(x, y)| {
                    assert_eq!(x.0, y.0, "{at}: harmonic");
                    (x.1 - y.1).abs().max((x.2 - y.2).abs())
                })
                .fold(0.0, f64::max);
            let level = rel(a.level, b.level);
            let f_stat = rel(a.f_stat, b.f_stat);
            let visible = (a.visible - b.visible).abs();
            let rss = rel(a.rss, b.rss);
            worst.level = worst.level.max(level);
            worst.f_stat = worst.f_stat.max(f_stat);
            worst.visible = worst.visible.max(visible);
            worst.rss = worst.rss.max(rss);
            worst.terms = worst.terms.max(terms);
            assert!(level < 1e-7, "{at}: level {} vs {}", a.level, b.level);
            assert!(f_stat < 1e-7, "{at}: f_stat {} vs {}", a.f_stat, b.f_stat);
            assert!(
                visible < 1e-9,
                "{at}: visible {} vs {}",
                a.visible,
                b.visible
            );
            assert!(rss < 1e-7, "{at}: rss {} vs {}", a.rss, b.rss);
            assert!(terms < 1e-7, "{at}: terms {:?} vs {:?}", a.terms, b.terms);
        }
        (a, b) => panic!("{at}: model {a:?} vs {b:?}"),
    }
    let (time, fd, t, _) = probe;
    let (c_new, c_old) = (new.correction(time, fd, t), old.correction(time, fd, t));
    worst.correction = worst.correction.max((c_new - c_old).abs());
    assert!(
        (c_new - c_old).abs() < 1e-7,
        "{at}: correction {c_new} vs {c_old}"
    );
}

#[test]
fn long_random_sequences_match_the_reference() {
    let mut worst = Worst::default();
    let mut significant_frames = 0usize;
    let mut tracked = 0usize;
    let mut frames = 0usize;
    for seed in 1..=24u64 {
        let mains = if seed % 3 == 0 { 60.0 } else { 50.0 };
        let mut seq = Seq::new(seed, mains);
        let harmonics = if seed % 5 == 0 { 2 } else { HARMONICS };
        let mut new = FlickerFit::new(mains, harmonics);
        let mut old = Reference::new(mains, harmonics);
        for i in 0..1500 {
            let (time, fd, t, level) = seq.next_frame(mains);
            new.add(time, fd, t, level);
            old.add(time, fd, t, level);
            frames += 1;
            if new.significant().is_some() {
                significant_frames += 1;
            }
            if (new.hz() - mains).abs() > 1e-9 {
                tracked += 1;
            }
            // Probe the next frame's brightness at a few exposures.
            let probe = (time + fd, fd, t, level);
            compare(
                &new,
                &old,
                probe,
                &mut worst,
                &format!("seed {seed} frame {i}"),
            );
        }
    }
    println!(
        "COMPARE flicker frames={frames} significant={significant_frames} tracked={tracked} \
         max_hz={:.3e} max_level_rel={:.3e} max_f_stat_rel={:.3e} max_visible={:.3e} \
         max_rss_rel={:.3e} max_terms={:.3e} max_correction={:.3e}",
        worst.hz,
        worst.level,
        worst.f_stat,
        worst.visible,
        worst.rss,
        worst.terms,
        worst.correction
    );
}

/// Per-call cost of `add` for the reference and the fit, on the module's three scenarios.
#[test]
#[ignore]
fn bench_flicker_add() {
    let half_wave = [(50.07, 0.25), (100.14, 0.1), (150.21, 0.03)];
    // (name, lamp components, fps, exposure)
    type Scenario<'a> = (&'a str, &'a [(f64, f64)], f64, f64);
    let scenarios: [Scenario; 3] = [
        ("strong_120fps", &half_wave, 120.0, 0.008),
        ("strong_30fps", &half_wave, 30.0, 0.025),
        ("none_120fps", &[], 120.0, 0.008),
    ];
    for (name, parts, fps, t) in scenarios {
        let fd = 1.0 / fps;
        let frame = |i: usize| {
            let t0 = i as f64 * fd + fd - t;
            (i as f64 * fd, fd, t, 0.3 * light(parts, t0, t))
        };
        let n = 2000;
        let mut new = FlickerFit::new(50.0, HARMONICS);
        let mut old = Reference::new(50.0, HARMONICS);
        for i in 0..200 {
            let (a, b, c, d) = frame(i);
            new.add(a, b, c, d);
            old.add(a, b, c, d);
        }
        let t_old = Instant::now();
        for i in 200..200 + n {
            let (a, b, c, d) = frame(i);
            old.add(a, b, c, d);
        }
        let per_old = t_old.elapsed().as_secs_f64() / n as f64 * 1e6;
        let t_new = Instant::now();
        for i in 200..200 + n {
            let (a, b, c, d) = frame(i);
            new.add(a, b, c, d);
        }
        let per_new = t_new.elapsed().as_secs_f64() / n as f64 * 1e6;
        println!("BENCH flicker_add {name} reference_us={per_old:.2} new_us={per_new:.2}");
    }
}
