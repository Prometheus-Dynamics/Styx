//! What AF reads from a frame: contrast and PDAF phase in the AF windows, the windows' mean
//! colour (scene-change detection) and the infrared test. From Raspberry Pi's `af.cpp`
//! (`computeWeights`, `getContrast`, `getPhase`, `getAverageAndTestIr`; BSD-2-Clause,
//! Copyright (C) 2022-2023 Raspberry Pi Ltd), on normalised floating-point statistics.

use alloc::{vec, vec::Vec};

use super::types::AfWindow;
use crate::stats::{PdafZone, Statistics, ZoneGrid};

/// At most this many windows are used (as libcamera).
pub const MAX_WINDOWS: usize = 10;

/// Zone weights for one grid size.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Weights {
    rows: u32,
    cols: u32,
    w: Vec<f64>,
    sum: f64,
}

impl Weights {
    /// Each window's area overlap with each zone, times its weight; the default window when
    /// there is none (or nothing overlaps).
    pub fn compute(rows: u32, cols: u32, windows: &[AfWindow]) -> Self {
        let (r, c) = (rows as usize, cols as usize);
        let mut w = vec![0.0; r * c];
        let mut sum = 0.0;
        if r > 0 && c > 0 {
            for win in windows.iter().take(MAX_WINDOWS) {
                let weight = win.weight.max(0.0);
                if weight == 0.0 || win.width <= 0.0 || win.height <= 0.0 {
                    continue;
                }
                let area = win.width * win.height;
                for row in 0..r {
                    let (y0, y1) = (row as f64 / r as f64, (row + 1) as f64 / r as f64);
                    let dy = y1.min(win.y + win.height) - y0.max(win.y);
                    if dy <= 0.0 {
                        continue;
                    }
                    for col in 0..c {
                        let (x0, x1) = (col as f64 / c as f64, (col + 1) as f64 / c as f64);
                        let dx = x1.min(win.x + win.width) - x0.max(win.x);
                        if dx <= 0.0 {
                            continue;
                        }
                        // Each window contributes its weight in total, spread by area.
                        let a = weight * dx * dy / area;
                        w[row * c + col] += a;
                        sum += a;
                    }
                }
            }
            if sum <= 0.0 {
                // The middle half of the width, middle third of the height.
                for row in r / 3..r - r / 3 {
                    for col in c / 4..c - c / 4 {
                        w[row * c + col] = 1.0;
                        sum += 1.0;
                    }
                }
            }
        }
        Self { rows, cols, w, sum }
    }

    pub fn fits<T>(&self, g: &ZoneGrid<T>) -> bool {
        self.rows == g.height && self.cols == g.width && self.w.len() == g.zones.len()
    }

    #[cfg(test)]
    pub fn total(&self) -> f64 {
        self.sum
    }
}

/// Cached weights per statistic, recomputed when the grid or the windows change.
#[derive(Debug, Clone, Default)]
pub(crate) struct Measure {
    windows: Vec<AfWindow>,
    contrast: Weights,
    phase: Weights,
    colour: Weights,
}

/// One frame's AF measurement.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Measurement {
    /// Weighted mean contrast (focus figure of merit) in the windows over the windows' squared
    /// green level; `None` without focus statistics.
    pub contrast: Option<f64>,
    /// Weighted PDAF phase and its confidence (0, 0 without usable phase data).
    pub phase: f64,
    pub conf: f64,
    /// Mean R, G, B in the windows.
    pub rgb: [f64; 3],
    /// The light looks infrared.
    pub ir: bool,
}

impl Measure {
    /// Use these windows (empty: the default area).
    pub fn set_windows(&mut self, windows: &[AfWindow]) {
        if self.windows != windows {
            self.windows = windows.to_vec();
            self.contrast = Weights::default();
            self.phase = Weights::default();
            self.colour = Weights::default();
        }
    }

    fn weights<'a, T>(
        cache: &'a mut Weights,
        windows: &[AfWindow],
        g: &ZoneGrid<T>,
    ) -> &'a Weights {
        if !cache.fits(g) {
            *cache = Weights::compute(g.height, g.width, windows);
        }
        cache
    }

    /// Measures one frame (`conf_thresh`, `conf_clip` as the tuning's; `check_ir` the IR test).
    pub fn measure(
        &mut self,
        stats: &Statistics,
        conf_thresh: f64,
        conf_clip: f64,
        check_ir: bool,
    ) -> Measurement {
        let mut m = Measurement::default();
        if let Some(f) = stats
            .focus
            .as_ref()
            .filter(|f| f.is_valid() && !f.is_empty())
        {
            let w = Self::weights(&mut self.contrast, &self.windows, f);
            let s: f64 = w.w.iter().zip(&f.zones).map(|(w, v)| w * v).sum();
            m.contrast = Some(if w.sum > 0.0 { s / w.sum } else { 0.0 });
        }
        if let Some(p) = stats
            .pdaf
            .as_ref()
            .filter(|p| p.is_valid() && !p.is_empty())
        {
            let w = Self::weights(&mut self.phase, &self.windows, p);
            (m.phase, m.conf) = phase(w, p, conf_thresh, conf_clip);
        }
        let c = &stats.colour;
        if c.is_valid() && !c.is_empty() {
            let w = Self::weights(&mut self.colour, &self.windows, c);
            let (mut sum, mut sw) = ([0.0; 3], 0.0);
            let (mut grey, mut all) = (0u64, 0u64);
            for (wt, z) in w.w.iter().zip(&c.zones) {
                let (r, g, b) = z.mean();
                if *wt > 0.0 && z.counted > 0 {
                    sum[0] += wt * r;
                    sum[1] += wt * g;
                    sum[2] += wt * b;
                    sw += wt;
                }
                if check_ir {
                    if close(r, g) && close(g, b) && close(r, b) {
                        grey += u64::from(z.counted);
                    }
                    all += u64::from(z.counted);
                }
            }
            if sw > 0.0 {
                m.rgb = sum.map(|v| v / sw);
            }
            let [r, g, b] = m.rgb;
            m.ir = check_ir && 2 * grey > all && close(r, g) && close(g, b) && close(r, b);
        }
        // Contrast relative to the windows' squared level: a figure of merit of gradient
        // energy then does not move with exposure and gain (AE settling during a scan).
        if let Some(c) = m.contrast.as_mut()
            && m.rgb[1] > 0.0
        {
            *c /= m.rgb[1] * m.rgb[1];
        }
        m
    }
}

/// Within 4:5 of each other.
fn close(a: f64, b: f64) -> bool {
    4.0 * a < 5.0 * b && 4.0 * b < 5.0 * a
}

/// Confidence-weighted phase over the cells at or above `thresh` (clipped at `clip`, less
/// half the threshold), and the confidence per unit weight; (0, 0) when the cells' confidence
/// does not reach the windows' total weight.
fn phase(w: &Weights, p: &ZoneGrid<PdafZone>, thresh: f64, clip: f64) -> (f64, f64) {
    let (mut sum_wc, mut sum_wcp) = (0.0, 0.0);
    for (wt, z) in w.w.iter().zip(&p.zones) {
        if *wt > 0.0 && z.conf >= thresh {
            let c = z.conf.min(clip) - thresh / 2.0;
            sum_wc += wt * c;
            sum_wcp += wt * c * z.phase;
        }
    }
    if w.sum > 0.0 && w.sum <= sum_wc {
        (sum_wcp / sum_wc, sum_wc / w.sum)
    } else {
        (0.0, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_area_and_windows() {
        let w = Weights::compute(8, 8, &[]);
        // Rows 2..6, columns 2..6.
        assert_eq!(w.total(), 16.0);
        assert_eq!(w.w[2 * 8 + 2], 1.0);
        assert_eq!(w.w[8 + 2], 0.0);
        // One window over the top-left quarter: the four zones there, weight 1 in total.
        let w = Weights::compute(4, 4, &[AfWindow::new(0.0, 0.0, 0.5, 0.5)]);
        assert!((w.total() - 1.0).abs() < 1e-12);
        assert!((w.w[0] - 0.25).abs() < 1e-12 && w.w[2] == 0.0);
        // A window outside the image: the default area.
        let w = Weights::compute(4, 4, &[AfWindow::new(2.0, 2.0, 0.5, 0.5)]);
        assert_eq!(w.total(), 4.0);
    }

    #[test]
    fn phase_needs_enough_confidence() {
        let w = Weights::compute(2, 2, &[AfWindow::new(0.0, 0.0, 1.0, 1.0)]);
        let mut g = ZoneGrid::<PdafZone>::new(2, 2);
        for z in &mut g.zones {
            *z = PdafZone {
                phase: 10.0,
                conf: 100.0,
            };
        }
        let (p, c) = phase(&w, &g, 16.0, 512.0);
        assert!((p - 10.0).abs() < 1e-12 && (c - 92.0).abs() < 1e-12);
        for z in &mut g.zones {
            z.conf = 4.0;
        }
        assert_eq!(phase(&w, &g, 16.0, 512.0), (0.0, 0.0));
    }
}
