//! Black level from dark frames, per channel and per gain, and hot pixels.
//!
//! Frames of one gain at one exposure (lens covered): the level is each channel's mean. Frames
//! of one gain at several exposures (lens covered or not, a static scene): each channel's mean
//! is fitted against exposure and the level is the fit at zero exposure, the way the OV9782's
//! level was checked on the CM5 without covering the lens. Hot pixels: samples of a covered
//! frame far above their channel's level.

use crate::raw::{RawFrame, frame_mean};

/// Levels measured at one gain.
#[derive(Clone, Debug, PartialEq)]
pub struct GainLevels {
    /// Analogue gain.
    pub gain: f64,
    /// R, Gr, Gb, B, normalised.
    pub levels: [f64; 4],
    /// Frames used.
    pub frames: usize,
    /// Extrapolated from several exposures (else the mean of covered frames).
    pub extrapolated: bool,
    /// Temporal noise of a covered frame at this gain (read noise), normalised; 0 when
    /// extrapolated.
    pub read_noise: f64,
}

/// The black level calibration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlackResult {
    /// In increasing gain.
    pub by_gain: Vec<GainLevels>,
    /// Hot pixels found in covered frames (full-resolution `(x, y)`).
    pub hot_pixels: Vec<(usize, usize)>,
    /// Samples examined for hot pixels.
    pub examined: usize,
}

impl BlackResult {
    /// Levels at a gain (linear between measured gains, nearest outside); `None` without data.
    pub fn at(&self, gain: f64) -> Option<[f64; 4]> {
        let l = &self.by_gain;
        let first = l.first()?;
        let i = l.iter().position(|e| e.gain >= gain);
        Some(match i {
            Some(0) => first.levels,
            None => l[l.len() - 1].levels,
            Some(i) => {
                let (a, b) = (&l[i - 1], &l[i]);
                let t = (gain - a.gain) / (b.gain - a.gain);
                std::array::from_fn(|c| a.levels[c] + (b.levels[c] - a.levels[c]) * t)
            }
        })
    }
}

fn channel_means(f: &RawFrame) -> [f64; 4] {
    let mut sum = [0.0; 4];
    let mut n = [0usize; 4];
    for y in 0..f.height {
        for x in 0..f.width {
            let c = f.cfa.channel(x, y);
            sum[c] += f64::from(f.data[y * f.width + x]);
            n[c] += 1;
        }
    }
    std::array::from_fn(|c| sum[c] / n[c].max(1) as f64 / f.full_scale())
}

/// Calibrate from every dark frame.
pub fn calibrate(frames: &[&RawFrame]) -> BlackResult {
    let mut gains: Vec<f64> = Vec::new();
    for f in frames {
        let g = (f.gain() * 1000.0).round() / 1000.0;
        if !gains.contains(&g) {
            gains.push(g);
        }
    }
    gains.sort_by(f64::total_cmp);
    let mut out = BlackResult::default();
    for g in gains {
        let group: Vec<&RawFrame> = frames
            .iter()
            .copied()
            .filter(|f| ((f.gain() * 1000.0).round() / 1000.0) == g)
            .collect();
        let mut exposures: Vec<f64> = group.iter().map(|f| f.exposure_us).collect();
        exposures.sort_by(f64::total_cmp);
        exposures.dedup_by(|a, b| (*a - *b).abs() < 0.5);
        let means: Vec<[f64; 4]> = group.iter().map(|f| channel_means(f)).collect();
        let (levels, extrapolated) = if exposures.len() >= 2 {
            // level = black + k × exposure, per channel.
            let xs: Vec<f64> = group.iter().map(|f| f.exposure_us).collect();
            let fit = |c: usize| {
                let ys: Vec<f64> = means.iter().map(|m| m[c]).collect();
                crate::linalg::polyfit(&xs, &ys, 1).map_or(ys[0], |k| k[0])
            };
            (std::array::from_fn(fit), true)
        } else {
            let n = means.len() as f64;
            (
                std::array::from_fn(|c| means.iter().map(|m| m[c]).sum::<f64>() / n),
                false,
            )
        };
        let mut read_noise = 0.0;
        if !extrapolated {
            read_noise = temporal_noise(&group);
            out.examined += group.len() * group[0].data.len();
            for f in &group {
                hot_pixels(f, &levels, &mut out.hot_pixels);
            }
        }
        out.by_gain.push(GainLevels {
            gain: g,
            levels,
            frames: group.len(),
            extrapolated,
            read_noise,
        });
    }
    out.hot_pixels.sort_unstable();
    out.hot_pixels.dedup();
    out
}

/// RMS of the frame-to-frame differences / √2 (the read noise), normalised.
fn temporal_noise(group: &[&RawFrame]) -> f64 {
    let pairs: Vec<f64> = group
        .windows(2)
        .map(|w| {
            let d: f64 = w[0]
                .data
                .iter()
                .zip(&w[1].data)
                .map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
                .sum();
            (d / w[0].data.len() as f64 / 2.0).sqrt() / w[0].full_scale()
        })
        .collect();
    if pairs.is_empty() {
        0.0
    } else {
        pairs.iter().sum::<f64>() / pairs.len() as f64
    }
}

/// Samples more than `max(16 codes, 8 σ)` above their channel's level.
fn hot_pixels(f: &RawFrame, levels: &[f64; 4], out: &mut Vec<(usize, usize)>) {
    let fs = f.full_scale();
    let mean = frame_mean(f);
    let var = f
        .data
        .iter()
        .map(|&v| (f64::from(v) / fs - mean).powi(2))
        .sum::<f64>()
        / f.data.len().max(1) as f64;
    let lim = (8.0 * var.sqrt()).max(16.0 / fs);
    for y in 0..f.height {
        for x in 0..f.width {
            let v = f64::from(f.data[y * f.width + x]) / fs;
            if v - levels[f.cfa.channel(x, y)] > lim {
                out.push((x, y));
            }
        }
    }
}
