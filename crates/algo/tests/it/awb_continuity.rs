//! The Bayesian AWB moves continuously with its statistics, and holds on to what it chose
//! while the statistics fit two temperatures about equally well.
//!
//! A warm lamp below the CT curve's range (the OV9782's room, recorded on the CM5) fits the
//! low end of the search almost exactly as well as the prior's peak at 6500 K: the colour
//! zones are far from grey at both, and the lux level decides how much the prior counts.
//! Scaling the statistics (and so the lux estimate) used to flip the estimate between
//! ~2500 K and ~6500 K at one point (2533 K → 4463 K for a 0.07% change); now it moves
//! through the tie continuously, and between frames the hysteresis keeps the side chosen.

use std::path::PathBuf;
use std::time::Duration;

use styx_algo::replay::Recording;
use styx_algo::tuning::{AwbMode, AwbPrior, AwbTuning, LuxTuning};
use styx_algo::{CameraConfig, FrameMetadata, Pipeline, Pwl, Statistics, StatsAccumulator, Tuning};

/// The estimates' colour temperatures for the statistics scaled by each factor, each from a
/// fresh pipeline (no history).
fn sweep(tuning: &Tuning, stats: &Statistics, meta: &FrameMetadata, ks: &[f64]) -> Vec<f64> {
    ks.iter()
        .map(|&k| {
            let mut p = Pipeline::from_tuning(tuning).unwrap();
            p.prepare(&CameraConfig::default()).unwrap();
            let s = scaled(stats, k);
            p.process(&s, meta).awb.estimate.0
        })
        .collect()
}

fn scaled(stats: &Statistics, k: f64) -> Statistics {
    let mut s = stats.clone();
    for z in &mut s.colour.zones {
        (z.r, z.g, z.b) = (z.r * k, z.g * k, z.b * k);
    }
    if let Some(l) = &mut s.luma {
        for z in &mut l.zones {
            z.y *= k;
        }
    }
    s
}

/// Largest step between neighbours, in mired.
fn largest_step(cts: &[f64]) -> f64 {
    cts.windows(2)
        .map(|w| (1e6 / w[0] - 1e6 / w[1]).abs())
        .fold(0.0, f64::max)
}

fn factors(lo: f64, hi: f64, step: f64) -> Vec<f64> {
    let n = ((hi - lo) / step).round() as usize;
    (0..=n).map(|i| lo + step * i as f64).collect()
}

/// A CT curve, priors and modes of the same shape as the OV9782's (not its values).
fn synthetic_tuning() -> Tuning {
    let awb = AwbTuning {
        ct_curve: vec![
            [2850.0, 0.95, 0.42],
            [3600.0, 0.83, 0.52],
            [4650.0, 0.68, 0.64],
            [5850.0, 0.62, 0.68],
            [7600.0, 0.50, 0.75],
        ],
        priors: vec![
            AwbPrior {
                lux: 0.0,
                prior: Pwl::from_flat(&[2000.0, 1.0, 3000.0, 0.0, 13000.0, 0.0]).unwrap(),
            },
            AwbPrior {
                lux: 800.0,
                prior: Pwl::from_flat(&[2000.0, 0.0, 6000.0, 2.0, 13000.0, 2.0]).unwrap(),
            },
            AwbPrior {
                lux: 1500.0,
                prior: Pwl::from_flat(&[
                    2000.0, 0.0, 4000.0, 1.0, 6000.0, 6.0, 6500.0, 7.0, 7000.0, 1.0, 13000.0, 1.0,
                ])
                .unwrap(),
            },
        ],
        modes: [(
            "auto".to_string(),
            AwbMode {
                lo: 2500.0,
                hi: 7700.0,
            },
        )]
        .into(),
        transverse_pos: 0.034,
        transverse_neg: 0.034,
        ..Default::default()
    };
    Tuning {
        awb: Some(awb),
        lux: Some(LuxTuning {
            reference_exposure_us: 20000.0,
            reference_gain: 1.0,
            reference_aperture: 1.0,
            reference_lux: 800.0,
            reference_y: 0.17,
        }),
        ..Default::default()
    }
}

/// 16x12 zones under a lamp at about 2200 K (R/G 1.12, B/G 0.31 for a grey), with a spread of
/// surface colours.
fn lamp_scene() -> Statistics {
    let mut a = StatsAccumulator::new(16, 12, 64, 1.0);
    for zy in 0..12u32 {
        for zx in 0..16u32 {
            let i = f64::from(zy * 16 + zx);
            // Deterministic pseudo-random surface tints.
            let tr = 1.0 + 0.6 * (i * 0.77).sin();
            let tb = 1.0 + 0.6 * (i * 1.31).cos();
            let level = 0.12 + 0.1 * (i * 0.13).sin().abs();
            for _ in 0..16 {
                a.add(zx, zy, level * 1.12 * tr, level, level * 0.31 * tb);
            }
        }
    }
    a.finish()
}

fn meta(frame: u64) -> FrameMetadata {
    FrameMetadata::new(
        frame,
        Duration::from_millis(20),
        1.0,
        Duration::from_millis(33),
    )
}

#[test]
fn the_estimate_is_continuous_in_the_statistics() {
    let tuning = synthetic_tuning();
    let stats = lamp_scene();
    let ks = factors(0.5, 2.5, 0.001);
    let cts = sweep(&tuning, &stats, &meta(5), &ks);
    let (lo, hi) = cts
        .iter()
        .fold((f64::MAX, f64::MIN), |(a, b), &c| (a.min(c), b.max(c)));
    // The sweep crosses the tie: dim, the lamp wins; bright, the prior's 6500 K.
    assert!(lo < 2800.0 && hi > 6000.0, "{lo} .. {hi}");
    let step = largest_step(&cts);
    assert!(step < 6.0, "largest step {step} mired");
}

/// Estimates (every frame) for frames alternating either side of the tie, after one frame
/// below it; mired.
fn alternating(tuning: &Tuning, stats: &Statistics, mid: f64, spread: f64) -> Vec<f64> {
    let mut p = Pipeline::from_tuning(&Tuning {
        awb: Some(AwbTuning {
            frame_period: 1,
            startup_frames: 1,
            ..tuning.awb.clone().unwrap()
        }),
        ..tuning.clone()
    })
    .unwrap();
    p.prepare(&CameraConfig::default()).unwrap();
    let below = scaled(stats, mid * (1.0 - spread));
    let above = scaled(stats, mid * (1.0 + spread));
    (0..40)
        .map(|f| {
            let s = if f % 2 == 0 { &below } else { &above };
            1e6 / p.process(s, &meta(f)).awb.estimate.0
        })
        .collect()
}

#[test]
fn hysteresis_keeps_the_side_chosen_through_noise() {
    let tuning = synthetic_tuning();
    let stats = lamp_scene();
    // Where the fresh estimate crosses 4000 K, half way between the two.
    let ks = factors(0.5, 2.5, 0.005);
    let cts = sweep(&tuning, &stats, &meta(5), &ks);
    let mid = ks[cts.iter().position(|&c| c > 4000.0).unwrap()];
    // Lux 10% either side of the tie, frame by frame: the estimate stays where it started.
    let held = alternating(&tuning, &stats, mid, 0.1);
    assert!(held[0] > 1e6 / 3000.0, "{}", 1e6 / held[0]);
    let drift = held.iter().map(|m| (m - held[0]).abs()).fold(0.0, f64::max);
    assert!(drift < 15.0, "{drift} mired: {held:?}");
    // Without hysteresis it follows each frame across.
    let mut free = tuning.clone();
    free.awb.as_mut().unwrap().hysteresis = 0.0;
    let flips = alternating(&free, &stats, mid, 0.1);
    let swing = flips
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f64::max);
    assert!(swing > 50.0, "{swing} mired");
    // A clear change (far above the tie) still moves at once.
    let mut p = Pipeline::from_tuning(&tuning).unwrap();
    p.prepare(&CameraConfig::default()).unwrap();
    p.process(&scaled(&stats, mid * 0.9), &meta(0));
    let e = p
        .process(&scaled(&stats, mid * 1.6), &meta(1))
        .awb
        .estimate
        .0;
    assert!(e > 6000.0, "{e}");
}

/// Next to this repository, or `STYX_LIBCAMERA_DIR`.
fn ov9782_tuning() -> Option<Tuning> {
    let root = std::env::var_os("STYX_LIBCAMERA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../libcamera"));
    let path = root.join("src/ipa/rpi/pisp/data/ov9782.json");
    if !path.exists() {
        eprintln!("skipped: no {}", path.display());
        return None;
    }
    Some(Tuning::load(path).unwrap())
}

#[test]
fn the_recorded_tie_is_continuous() {
    let Some(tuning) = ov9782_tuning() else {
        return;
    };
    let rec = Recording::read(include_str!("../data/awb-tie.jsonl").as_bytes()).unwrap();
    let r = &rec.records[0];
    let ks = factors(0.5, 1.5, 0.0005);
    let cts = sweep(&tuning, &r.stats, &r.meta, &ks);
    let (lo, hi) = cts
        .iter()
        .fold((f64::MAX, f64::MIN), |(a, b), &c| (a.min(c), b.max(c)));
    assert!(lo < 2700.0 && hi > 6000.0, "{lo} .. {hi}");
    let step = largest_step(&cts);
    assert!(step < 6.0, "largest step {step} mired");
    // Around the factor that used to flip the result.
    let near = sweep(&tuning, &r.stats, &r.meta, &factors(0.99, 1.01, 0.0001));
    assert!(largest_step(&near) < 1.0, "{}", largest_step(&near));
}
