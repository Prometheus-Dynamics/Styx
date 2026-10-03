//! The noise profile: noise (standard deviation) against signal, in the Raspberry Pi model the
//! denoise blocks use, `σ = constant + slope × √level` on the 16-bit scale at analogue gain 1
//! (the IPA scales both with √gain).
//!
//! Samples come from the temporal variance of bursts (several frames at the same settings of a
//! static scene: per pixel, after scaling out frame-to-frame brightness changes such as lamp
//! flicker; no fixed pattern or texture counts as noise) binned by level, or, for single
//! frames, from the variance inside the chart's patches after removing a plane (`ctt`'s
//! source; it also counts the print's texture and pixel response non-uniformity). Samples
//! taken at gain `g` are divided by `√g`. The fit drops outliers (3 robust deviations) and,
//! like `ctt`, refits through the origin when the constant comes out negative.

use crate::raw::{Burst, Planes};

/// One noise measurement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoiseSample {
    /// Signal above black, 16-bit scale.
    pub level: f64,
    /// Standard deviation, 16-bit scale, at the gain it was measured.
    pub sigma: f64,
    /// Analogue × digital gain.
    pub gain: f64,
    /// Pixels behind it.
    pub count: usize,
}

/// A fitted profile.
#[derive(Clone, Debug, PartialEq)]
pub struct NoiseFit {
    /// Constant, 16-bit scale, at gain 1.
    pub constant: f64,
    /// Slope at gain 1.
    pub slope: f64,
    /// Samples used / offered.
    pub used: usize,
    /// Samples offered.
    pub offered: usize,
    /// RMS relative error of the fit over the samples used.
    pub rms_error: f64,
}

const FULL: f64 = 65536.0;

/// Samples from a burst's temporal variance: `levels` (black removed) binned by level. Each
/// bin's variance is the median of its pixels' variances (corrected for the median of a χ²
/// distribution), so pixels that change for other reasons (edges of a scene that moves a
/// little, a blinking LED, a lamp in view) do not count.
pub fn temporal_samples(burst: &Burst, levels: &Planes) -> Vec<NoiseSample> {
    let Some(var) = &burst.variance else {
        return Vec::new();
    };
    const BINS: usize = 64;
    let (lo, hi) = (0.002, 0.9);
    let mut bins: Vec<(f64, Vec<f32>)> = vec![(0.0, Vec::new()); BINS];
    for c in 0..4 {
        for (l, v) in levels.ch[c].iter().zip(&var.ch[c]) {
            let l = f64::from(*l);
            if l <= lo || l >= hi {
                continue;
            }
            // Log-spaced bins: noise samples cover dark and bright parts evenly.
            let k = (((l / lo).ln() / (hi / lo).ln()) * BINS as f64) as usize;
            let b = &mut bins[k.min(BINS - 1)];
            b.0 += l;
            b.1.push(*v);
        }
    }
    // Median of χ²(k) / k, k = frames - 1 (Wilson-Hilferty).
    let k = (burst.frames.max(2) - 1) as f64;
    let chi_median = (1.0 - 2.0 / (9.0 * k)).powi(3);
    bins.into_iter()
        .filter(|b| b.1.len() >= 200)
        .map(|(l, mut v)| {
            let n = v.len();
            let med = f64::from(crate::raw::median(&mut v)) / chi_median;
            NoiseSample {
                level: l / n as f64 * FULL,
                sigma: med.sqrt() * FULL,
                gain: burst.gain(),
                count: n,
            }
        })
        .collect()
}

/// Fit the profile at gain 1.
pub fn fit(samples: &[NoiseSample]) -> Option<NoiseFit> {
    let pts: Vec<(f64, f64, f64)> = samples
        .iter()
        .filter(|s| s.level > 0.0 && s.sigma > 0.0)
        .map(|s| {
            (
                s.level.sqrt(),
                s.sigma / s.gain.sqrt(),
                (s.count as f64).sqrt(),
            )
        })
        .collect();
    if pts.len() < 3 {
        return None;
    }
    let solve = |p: &[(f64, f64, f64)]| -> Option<(f64, f64)> {
        let rows: Vec<Vec<f64>> = p.iter().map(|q| vec![1.0, q.0]).collect();
        let y: Vec<f64> = p.iter().map(|q| q.1).collect();
        let w: Vec<f64> = p.iter().map(|q| q.2).collect();
        let k = crate::linalg::lstsq(&rows, &y, Some(&w))?;
        if k[0] >= 0.0 {
            Some((k[0], k[1]))
        } else {
            let num: f64 = p.iter().map(|q| q.2 * q.0 * q.1).sum();
            let den: f64 = p.iter().map(|q| q.2 * q.0 * q.0).sum();
            Some((0.0, num / den))
        }
    };
    let (c, s) = solve(&pts)?;
    let res: Vec<f64> = pts.iter().map(|q| q.1 - (c + s * q.0)).collect();
    let mut abs: Vec<f32> = res.iter().map(|r| r.abs() as f32).collect();
    let mad = f64::from(crate::raw::median(&mut abs)) * 1.4826;
    let kept: Vec<(f64, f64, f64)> = pts
        .iter()
        .zip(&res)
        .filter(|(_, r)| r.abs() <= 3.0 * mad.max(1e-9))
        .map(|(q, _)| *q)
        .collect();
    let kept = if kept.len() >= 3 { kept } else { pts.clone() };
    let (c, s) = solve(&kept)?;
    let rms = (kept
        .iter()
        .map(|q| ((c + s * q.0) / q.1 - 1.0).powi(2))
        .sum::<f64>()
        / kept.len() as f64)
        .sqrt();
    Some(NoiseFit {
        constant: (c * 100.0).round() / 100.0,
        slope: (s * 1000.0).round() / 1000.0,
        used: kept.len(),
        offered: samples.len(),
        rms_error: rms,
    })
}
