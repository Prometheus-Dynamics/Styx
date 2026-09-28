//! Whole-frame analysis for the verification runs: unpacked levels, per-row profiles (to find
//! optically black or otherwise special rows), Bayer-phase means, percentiles, and a
//! least-squares line fit for the exposure and gain sweeps.

use crate::frames::{Raw10Layout, unpack_raw10_line};

/// Levels of one frame, unpacked to 10 bits.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameLevels {
    /// Pixels per line.
    pub width: usize,
    /// Lines.
    pub height: usize,
    /// Row-major 10-bit values.
    pub pixels: Vec<u16>,
}

impl FrameLevels {
    /// Unpacks a packed raw10 frame.
    pub fn unpack(layout: &Raw10Layout, data: &[u8]) -> std::io::Result<Self> {
        // `mean_level` validates the geometry against the buffer.
        layout.mean_level(data, layout.height.max(1))?;
        let mut pixels = Vec::with_capacity(layout.width * layout.height);
        let mut line = Vec::with_capacity(layout.width);
        for y in 0..layout.height {
            unpack_raw10_line(&data[y * layout.stride..], layout.width, &mut line);
            pixels.extend_from_slice(&line);
        }
        Ok(Self {
            width: layout.width,
            height: layout.height,
            pixels,
        })
    }

    fn rows(&self, from: usize, to: usize) -> &[u16] {
        &self.pixels[from * self.width..to * self.width]
    }

    /// Mean of every row.
    pub fn row_means(&self) -> Vec<f64> {
        self.pixels
            .chunks_exact(self.width)
            .map(|r| r.iter().map(|&v| f64::from(v)).sum::<f64>() / r.len() as f64)
            .collect()
    }

    /// Statistics over rows `from..to`.
    pub fn stats(&self, from: usize, to: usize) -> LevelStats {
        LevelStats::of(self.rows(from.min(self.height), to.min(self.height)))
    }

    /// Means of the four 2x2 phases (top-left, top-right, bottom-left, bottom-right) over rows
    /// `from..to`: for BGGR, B, Gb, Gr, R.
    pub fn phase_means(&self, from: usize, to: usize) -> [f64; 4] {
        let mut sum = [0u64; 4];
        let mut n = [0u64; 4];
        for y in from..to.min(self.height) {
            let row = &self.pixels[y * self.width..][..self.width];
            for (x, &v) in row.iter().enumerate() {
                let p = (y % 2) * 2 + x % 2;
                sum[p] += u64::from(v);
                n[p] += 1;
            }
        }
        std::array::from_fn(|i| {
            if n[i] == 0 {
                0.0
            } else {
                sum[i] as f64 / n[i] as f64
            }
        })
    }
}

/// Distribution of a set of levels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelStats {
    /// Mean.
    pub mean: f64,
    /// Standard deviation.
    pub std: f64,
    /// Minimum.
    pub min: u16,
    /// 1st percentile.
    pub p01: u16,
    /// Median.
    pub p50: u16,
    /// 99th percentile.
    pub p99: u16,
    /// Maximum.
    pub max: u16,
    /// Fraction of values at 1023 (saturated).
    pub saturated: f64,
    /// Fraction of values at 0.
    pub zero: f64,
}

impl LevelStats {
    /// Statistics of `values` (all zero when empty).
    pub fn of(values: &[u16]) -> Self {
        if values.is_empty() {
            return Self {
                mean: 0.0,
                std: 0.0,
                min: 0,
                p01: 0,
                p50: 0,
                p99: 0,
                max: 0,
                saturated: 0.0,
                zero: 0.0,
            };
        }
        let mut hist = [0u64; 1024];
        let mut sum = 0f64;
        let mut sq = 0f64;
        for &v in values {
            hist[usize::from(v.min(1023))] += 1;
            let f = f64::from(v);
            sum += f;
            sq += f * f;
        }
        let n = values.len() as f64;
        let mean = sum / n;
        let pct = |p: f64| -> u16 {
            let target = (p * (n - 1.0)).round() as u64;
            let mut seen = 0;
            for (v, &c) in hist.iter().enumerate() {
                seen += c;
                if seen > target {
                    return v as u16;
                }
            }
            1023
        };
        Self {
            mean,
            std: (sq / n - mean * mean).max(0.0).sqrt(),
            min: pct(0.0),
            p01: pct(0.01),
            p50: pct(0.5),
            p99: pct(0.99),
            max: pct(1.0),
            saturated: hist[1023] as f64 / n,
            zero: hist[0] as f64 / n,
        }
    }
}

impl std::fmt::Display for LevelStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "mean {:.2} sd {:.2} min {} p1 {} p50 {} p99 {} max {} (zero {:.1}%, sat {:.1}%)",
            self.mean,
            self.std,
            self.min,
            self.p01,
            self.p50,
            self.p99,
            self.max,
            self.zero * 100.0,
            self.saturated * 100.0
        )
    }
}

/// A run of consecutive rows with similar means.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowBand {
    /// First row.
    pub start: usize,
    /// One past the last row.
    pub end: usize,
    /// Mean over the band.
    pub mean: f64,
}

/// Splits row means into bands: a new band starts where a row differs from the running band
/// mean by more than `tolerance`.
pub fn row_bands(rows: &[f64], tolerance: f64) -> Vec<RowBand> {
    let mut bands: Vec<RowBand> = Vec::new();
    let mut sum = 0.0;
    for (i, &m) in rows.iter().enumerate() {
        match bands.last_mut() {
            Some(b) if (m - b.mean).abs() <= tolerance => {
                sum += m;
                b.end = i + 1;
                b.mean = sum / (b.end - b.start) as f64;
            }
            _ => {
                sum = m;
                bands.push(RowBand {
                    start: i,
                    end: i + 1,
                    mean: m,
                });
            }
        }
    }
    bands
}

/// A least-squares line `y = slope * x + intercept`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineFit {
    /// Slope.
    pub slope: f64,
    /// Intercept.
    pub intercept: f64,
    /// Coefficient of determination.
    pub r2: f64,
    /// Largest absolute residual relative to the fitted range of y.
    pub max_rel_residual: f64,
}

/// Fits a line through `(x, y)` points; `None` with fewer than two distinct x.
pub fn fit_line(points: &[(f64, f64)]) -> Option<LineFit> {
    let n = points.len() as f64;
    if points.len() < 2 {
        return None;
    }
    let mx = points.iter().map(|p| p.0).sum::<f64>() / n;
    let my = points.iter().map(|p| p.1).sum::<f64>() / n;
    let sxx: f64 = points.iter().map(|p| (p.0 - mx).powi(2)).sum();
    if sxx == 0.0 {
        return None;
    }
    let sxy: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let slope = sxy / sxx;
    let intercept = my - slope * mx;
    let syy: f64 = points.iter().map(|p| (p.1 - my).powi(2)).sum();
    let res: Vec<f64> = points
        .iter()
        .map(|p| p.1 - (slope * p.0 + intercept))
        .collect();
    let ss_res: f64 = res.iter().map(|r| r * r).sum();
    let r2 = if syy == 0.0 { 1.0 } else { 1.0 - ss_res / syy };
    let (lo, hi) = points
        .iter()
        .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
    let span = (hi - lo).max(f64::EPSILON);
    let max_rel_residual = res.iter().fold(0.0f64, |a, r| a.max(r.abs())) / span;
    Some(LineFit {
        slope,
        intercept,
        r2,
        max_rel_residual,
    })
}

/// Logs a frame's row profile as bands, its overall statistics and phase means.
pub fn describe_frame(label: &str, f: &FrameLevels) {
    let bands = row_bands(&f.row_means(), 3.0);
    crate::log!("{label}: {}x{} {}", f.width, f.height, f.stats(0, f.height));
    for b in bands.iter().take(12) {
        crate::log!(
            "{label}:   rows {:>3}..{:<3} mean {:7.2}  {}",
            b.start,
            b.end,
            b.mean,
            f.stats(b.start, b.end)
        );
    }
    if bands.len() > 12 {
        crate::log!("{label}:   ... {} bands in all", bands.len());
    }
    let p = f.phase_means(0, f.height);
    crate::log!(
        "{label}: 2x2 phase means {:.2} {:.2} / {:.2} {:.2}",
        p[0],
        p[1],
        p[2],
        p[3]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_and_percentiles() {
        let v: Vec<u16> = (0..=100).collect();
        let s = LevelStats::of(&v);
        assert_eq!((s.min, s.p01, s.p50, s.p99, s.max), (0, 1, 50, 99, 100));
        assert!((s.mean - 50.0).abs() < 1e-9);
        assert!((s.zero - 1.0 / 101.0).abs() < 1e-9);
        assert_eq!(LevelStats::of(&[]).max, 0);
        let sat = LevelStats::of(&[1023, 1023, 0, 5]);
        assert_eq!((sat.saturated, sat.zero), (0.5, 0.25));
    }

    #[test]
    fn bands_split_on_steps() {
        let mut rows = vec![85.0; 32];
        rows.extend(vec![0.0; 768]);
        let b = row_bands(&rows, 3.0);
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].start, b[0].end, b[1].end), (0, 32, 800));
        assert!((b[0].mean - 85.0).abs() < 1e-9);
    }

    #[test]
    fn line_fits() {
        let pts: Vec<(f64, f64)> = (1..=8)
            .map(|x| (x as f64, 64.0 + 20.0 * x as f64))
            .collect();
        let f = fit_line(&pts).unwrap();
        assert!((f.slope - 20.0).abs() < 1e-9 && (f.intercept - 64.0).abs() < 1e-9);
        assert!((f.r2 - 1.0).abs() < 1e-12 && f.max_rel_residual < 1e-9);
        assert!(fit_line(&[(1.0, 2.0)]).is_none());
        assert!(fit_line(&[(1.0, 2.0), (1.0, 3.0)]).is_none());
    }

    #[test]
    fn unpacks_whole_frames_and_phases() {
        let layout = Raw10Layout {
            width: 4,
            height: 2,
            stride: 8,
        };
        // Row 0: 100 200 100 200, row 1: 300 400 300 400 (packed raw10).
        let pack = |p: [u16; 4]| {
            let low = p
                .iter()
                .enumerate()
                .fold(0u8, |a, (i, v)| a | (((v & 3) as u8) << (2 * i)));
            let mut out: Vec<u8> = p.iter().map(|v| (v >> 2) as u8).collect();
            out.push(low);
            out.extend([0, 0, 0]);
            out
        };
        let mut data = pack([100, 200, 100, 200]);
        data.extend(pack([300, 400, 300, 400]));
        let f = FrameLevels::unpack(&layout, &data).unwrap();
        assert_eq!(f.pixels, [100, 200, 100, 200, 300, 400, 300, 400]);
        assert_eq!(f.row_means(), [150.0, 350.0]);
        assert_eq!(f.phase_means(0, 2), [100.0, 200.0, 300.0, 400.0]);
        assert!(FrameLevels::unpack(&layout, &data[..10]).is_err());
    }
}
