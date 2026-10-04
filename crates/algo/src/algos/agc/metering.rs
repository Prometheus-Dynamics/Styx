//! Metering: zone weights and the weighted mean luma after a trial gain.

use alloc::vec::Vec;

#[cfg(not(feature = "std"))]
use crate::math::Float as _;

use crate::stats::{Histogram, Statistics, ZoneGrid, rec601};

use super::tuning::MeteringMode;

/// Weights for a `w × h` statistics grid: from the tuning when its grid can be mapped, else the
/// built-in weights for the mode's name.
pub(crate) fn weights_for(name: &str, tuned: Option<&MeteringMode>, w: u32, h: u32) -> Vec<f64> {
    let n = (w * h) as usize;
    if let Some(m) = tuned {
        let grid = m.grid.or_else(|| {
            if m.weights.len() == n {
                Some((w, h))
            } else {
                let s = (m.weights.len() as f64).sqrt().round() as u32;
                ((s * s) as usize == m.weights.len()).then_some((s, s))
            }
        });
        if let Some((gw, gh)) = grid {
            return resample(&m.weights, gw, gh, w, h);
        }
    }
    builtin(name, w, h)
}

/// The weights of a metering mode on their own grid (the tuning's, else 15×15 built-in
/// weights), for an ISP that weights its histogram by zone.
pub(crate) fn histogram_grid(name: &str, tuned: Option<&MeteringMode>) -> ZoneGrid<f64> {
    if let Some(m) = tuned {
        let s = (m.weights.len() as f64).sqrt().round() as u32;
        let grid = m
            .grid
            .or_else(|| ((s * s) as usize == m.weights.len()).then_some((s, s)));
        if let Some((width, height)) = grid {
            return ZoneGrid {
                width,
                height,
                zones: m.weights.clone(),
            };
        }
    }
    ZoneGrid {
        width: 15,
        height: 15,
        zones: builtin(name, 15, 15),
    }
}

/// Nearest-neighbour resampling of a weight grid by zone centres.
fn resample(weights: &[f64], gw: u32, gh: u32, w: u32, h: u32) -> Vec<f64> {
    if (gw, gh) == (w, h) {
        return weights.to_vec();
    }
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        let sy = (((f64::from(y) + 0.5) / f64::from(h)) * f64::from(gh)) as u32;
        for x in 0..w {
            let sx = (((f64::from(x) + 0.5) / f64::from(w)) * f64::from(gw)) as u32;
            out.push(weights[(sy.min(gh - 1) * gw + sx.min(gw - 1)) as usize]);
        }
    }
    out
}

/// Built-in weights: centre-weighted (1 at the edges to 4 in the middle), spot (the central
/// ~15% of the frame) or average (uniform, also for unknown names).
fn builtin(name: &str, w: u32, h: u32) -> Vec<f64> {
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let dx = (f64::from(x) + 0.5) / f64::from(w) * 2.0 - 1.0;
            let dy = (f64::from(y) + 0.5) / f64::from(h) * 2.0 - 1.0;
            let r = (dx * dx + dy * dy).sqrt();
            out.push(match name {
                "centre-weighted" | "center-weighted" => 1.0 + 3.0 * (1.0 - r).max(0.0),
                "spot" => {
                    let spot = (1.0 / f64::from(w.min(h))).max(0.15);
                    if r <= spot { 1.0 } else { 0.0 }
                }
                _ => 1.0,
            });
        }
    }
    if out.iter().all(|w| *w == 0.0) {
        out.iter_mut().for_each(|w| *w = 1.0);
    }
    out
}

/// Weighted mean luma if the image had `gain` more exposure, with each zone clipped at full
/// scale (so the estimate is non-linear once zones saturate).
///
/// Ported from `computeInitialY` in Raspberry Pi's `agc_channel.cpp` (BSD-2-Clause,
/// Copyright (C) 2023 Raspberry Pi Ltd).
pub(crate) fn weighted_y(
    stats: &Statistics,
    weights: &[f64],
    use_luma: bool,
    wb: [f64; 3],
    gain: f64,
) -> f64 {
    if use_luma && let Some(l) = &stats.luma {
        let (mut sum, mut pixels) = (0.0, 0.0);
        for (z, w) in l.zones.iter().zip(weights) {
            let n = f64::from(z.counted) * w;
            sum += (z.y * w * gain).min(n);
            pixels += n;
        }
        return if pixels == 0.0 { 0.0 } else { sum / pixels };
    }
    if stats.colour.is_empty() && !stats.histogram.is_empty() {
        return histogram_y(&stats.histogram, gain);
    }
    let (mut r, mut g, mut b, mut pixels) = (0.0, 0.0, 0.0, 0.0);
    for (z, w) in stats.colour.zones.iter().zip(weights) {
        let n = f64::from(z.counted) * w;
        r += (z.r * w * gain).min(n);
        g += (z.g * w * gain).min(n);
        b += (z.b * w * gain).min(n);
        pixels += n;
    }
    if pixels == 0.0 {
        // Nothing usable (e.g. every zone saturated): read the histogram, else assume saturation.
        return if stats.histogram.total() > 0 {
            histogram_y(&stats.histogram, gain)
        } else {
            1.0
        };
    }
    if stats.before_wb {
        (r, g, b) = (r * wb[0], g * wb[1], b * wb[2]);
    }
    rec601(r, g, b) / pixels
}

/// Mean luma from the histogram alone: values below full scale / gain scale with the gain, the
/// rest saturate.
fn histogram_y(h: &Histogram, gain: f64) -> f64 {
    let bins = h.len() as f64;
    let total = h.total() as f64;
    if total == 0.0 {
        return 0.0;
    }
    let min_bin = (1.0 / gain).min(1.0) * bins;
    let mean = h.inter_bin_mean(0.0, min_bin);
    let unsaturated = h.cumulative_freq(min_bin);
    let sum = mean * gain * unsaturated + (total - unsaturated) * bins;
    sum / total / bins
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::StatsAccumulator;

    #[test]
    fn builtin_weights_have_expected_shape() {
        let cw = builtin("centre-weighted", 5, 5);
        assert!(cw[12] > cw[0]);
        let spot = builtin("spot", 5, 5);
        assert_eq!(spot.iter().filter(|w| **w > 0.0).count(), 1);
        assert!(builtin("matrix", 3, 3).iter().all(|w| *w == 1.0));
    }

    #[test]
    fn tuned_square_weights_resample() {
        let m = MeteringMode {
            weights: vec![0.0, 1.0, 2.0, 3.0],
            grid: None,
        };
        let w = weights_for("x", Some(&m), 4, 4);
        assert_eq!(w[0], 0.0);
        assert_eq!(w[15], 3.0);
        // 15 weights (the old VC4 layout) cannot be mapped: built-in weights instead.
        let vc4 = MeteringMode {
            weights: vec![1.0; 15],
            grid: None,
        };
        assert_eq!(weights_for("spot", Some(&vc4), 5, 5), builtin("spot", 5, 5));
    }

    #[test]
    fn weighted_y_clips_saturating_zones() {
        let mut a = StatsAccumulator::new(2, 1, 16, 2.0);
        a.add(0, 0, 0.1, 0.1, 0.1);
        a.add(1, 0, 0.6, 0.6, 0.6);
        let s = a.finish();
        let w = [1.0, 1.0];
        assert!((weighted_y(&s, &w, true, [1.0; 3], 1.0) - 0.35).abs() < 1e-12);
        // Gain 2: 0.2 and 1.2 clipped to 1.0.
        assert!((weighted_y(&s, &w, true, [1.0; 3], 2.0) - 0.6).abs() < 1e-12);
        assert!((weighted_y(&s, &w, false, [1.0; 3], 2.0) - 0.6).abs() < 1e-12);
        let only_hist = Statistics {
            histogram: s.histogram.clone(),
            ..Default::default()
        };
        assert!(histogram_y(&only_hist.histogram, 1.0) > 0.3);
    }
}
