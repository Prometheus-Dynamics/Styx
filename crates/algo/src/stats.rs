//! Statistics: the plain-data input every algorithm reads.
//!
//! Units are hardware independent: every pixel value is normalised so full scale is `1.0`, with
//! the black level already removed. A zone holds *sums* of such values plus the number of pixels
//! counted, so converters add up what the hardware reports and scale once:
//!
//! * PiSP front end: `sum / 2^bits_of_the_stat` per channel, `counted` as reported (AWB regions
//!   become [`Statistics::colour`], AGC regions [`Statistics::luma`], the Y histogram
//!   [`Statistics::histogram`], the CDAF regions [`Statistics::focus`]).
//! * A software ISP: accumulate demosaiced or per-Bayer-quad values with [`StatsAccumulator`].

use serde::{Deserialize, Serialize};

/// Rec.601 luma of linear RGB.
pub fn rec601(r: f64, g: f64, b: f64) -> f64 {
    0.299 * r + 0.587 * g + 0.114 * b
}

/// One colour zone: sums of normalised pixel values.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ColourZone {
    /// Sum of red.
    pub r: f64,
    /// Sum of green.
    pub g: f64,
    /// Sum of blue.
    pub b: f64,
    /// Pixels counted (e.g. not saturated).
    pub counted: u32,
}

impl ColourZone {
    /// Mean (r, g, b), zero when nothing was counted.
    pub fn mean(&self) -> (f64, f64, f64) {
        if self.counted == 0 {
            return (0.0, 0.0, 0.0);
        }
        let n = f64::from(self.counted);
        (self.r / n, self.g / n, self.b / n)
    }
}

/// One luma zone.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct LumaZone {
    /// Sum of luma.
    pub y: f64,
    /// Pixels counted.
    pub counted: u32,
}

/// A row-major grid of zones, `zones.len() == width * height`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ZoneGrid<T> {
    /// Zones across.
    pub width: u32,
    /// Zones down.
    pub height: u32,
    /// Row-major zones.
    pub zones: Vec<T>,
}

impl<T: Clone + Default> ZoneGrid<T> {
    /// A grid of default zones.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            zones: vec![T::default(); (width * height) as usize],
        }
    }
}

impl<T> ZoneGrid<T> {
    /// Number of zones.
    pub fn len(&self) -> usize {
        self.zones.len()
    }

    /// No zones.
    pub fn is_empty(&self) -> bool {
        self.zones.is_empty()
    }

    /// The grid is consistent.
    pub fn is_valid(&self) -> bool {
        self.zones.len() == (self.width as usize) * (self.height as usize)
    }
}

/// A histogram over normalised luma `[0, 1)`, bins of equal width.
///
/// Quantile arithmetic ported from Raspberry Pi's `controller/histogram.cpp` (BSD-2-Clause,
/// Copyright (C) 2019 Raspberry Pi Ltd).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(from = "Vec<u64>", into = "Vec<u64>")]
pub struct Histogram {
    bins: Vec<u64>,
    cumulative: Vec<u64>,
}

impl From<Vec<u64>> for Histogram {
    fn from(bins: Vec<u64>) -> Self {
        let mut cumulative = Vec::with_capacity(bins.len() + 1);
        let mut acc = 0u64;
        cumulative.push(0);
        for &b in &bins {
            acc += b;
            cumulative.push(acc);
        }
        Self { bins, cumulative }
    }
}

impl From<Histogram> for Vec<u64> {
    fn from(h: Histogram) -> Self {
        h.bins
    }
}

impl Histogram {
    /// Bin counts.
    pub fn bins(&self) -> &[u64] {
        &self.bins
    }

    /// Number of bins.
    pub fn len(&self) -> usize {
        self.bins.len()
    }

    /// No bins.
    pub fn is_empty(&self) -> bool {
        self.bins.is_empty()
    }

    /// Total count.
    pub fn total(&self) -> u64 {
        self.cumulative.last().copied().unwrap_or(0)
    }

    /// Count below the fractional bin position `bin`.
    pub fn cumulative_freq(&self, bin: f64) -> f64 {
        if bin <= 0.0 || self.bins.is_empty() {
            return 0.0;
        }
        if bin >= self.bins.len() as f64 {
            return self.total() as f64;
        }
        let b = bin as usize;
        let (c0, c1) = (self.cumulative[b] as f64, self.cumulative[b + 1] as f64);
        c0 + (bin - b as f64) * (c1 - c0)
    }

    /// Fractional bin position of quantile `q` in `[0, 1]`.
    pub fn quantile(&self, q: f64) -> f64 {
        self.quantile_from(q, 0)
    }

    fn quantile_from(&self, q: f64, first: usize) -> f64 {
        if self.bins.is_empty() {
            return 0.0;
        }
        let items = (q.clamp(0.0, 1.0) * self.total() as f64) as u64;
        let (mut first, mut last) = (first.min(self.bins.len() - 1), self.bins.len() - 1);
        while first < last {
            let middle = (first + last) / 2;
            if self.cumulative[middle + 1] > items {
                last = middle;
            } else {
                first = middle + 1;
            }
        }
        let (c0, c1) = (self.cumulative[first], self.cumulative[first + 1]);
        let frac = if c1 == c0 {
            0.0
        } else {
            (items.saturating_sub(c0)) as f64 / (c1 - c0) as f64
        };
        first as f64 + frac
    }

    /// Mean bin position (bin centres) between two fractional bin positions.
    pub fn inter_bin_mean(&self, mut lo: f64, hi: f64) -> f64 {
        let (mut sum, mut count) = (0.0, 0.0);
        let mut next = lo.floor() + 1.0;
        while next <= hi.ceil() {
            let bin = lo.floor() as usize;
            if bin >= self.bins.len() {
                break;
            }
            let freq = self.bins[bin] as f64 * (next.min(hi) - lo);
            sum += bin as f64 * freq;
            count += freq;
            lo = next;
            next += 1.0;
        }
        if count == 0.0 { hi } else { sum / count + 0.5 }
    }

    /// Mean bin position between two quantiles.
    pub fn inter_quantile_mean(&self, q_lo: f64, q_hi: f64) -> f64 {
        let lo = self.quantile(q_lo);
        let hi = self.quantile_from(q_hi, lo as usize);
        self.inter_bin_mean(lo, hi)
    }
}

/// Everything the algorithms read from one frame's statistics.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Statistics {
    /// Colour zones (AWB, ALSC, and AGC when there is no luma grid).
    pub colour: ZoneGrid<ColourZone>,
    /// Luma zones for AGC metering, if the hardware provides a separate grid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub luma: Option<ZoneGrid<LumaZone>>,
    /// Luma histogram.
    pub histogram: Histogram,
    /// Focus figures of merit per zone, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<ZoneGrid<f64>>,
    /// Colour statistics were taken before white-balance gains (true for PiSP and most ISPs).
    #[serde(default = "yes")]
    pub before_wb: bool,
    /// Colour statistics were taken before lens-shading correction.
    #[serde(default)]
    pub before_lsc: bool,
}

fn yes() -> bool {
    true
}

impl Statistics {
    /// Mean luma of the whole image (no white balance applied), from the luma grid if present.
    pub fn mean_luma(&self) -> f64 {
        let (sum, n) = match &self.luma {
            Some(l) if !l.is_empty() => l
                .zones
                .iter()
                .fold((0.0, 0u64), |(s, n), z| (s + z.y, n + u64::from(z.counted))),
            _ => self.colour.zones.iter().fold((0.0, 0u64), |(s, n), z| {
                (s + rec601(z.r, z.g, z.b), n + u64::from(z.counted))
            }),
        };
        if n == 0 { 0.0 } else { sum / n as f64 }
    }
}

/// Builds [`Statistics`] from pixel values (for software ISPs and the simulator).
#[derive(Debug, Clone)]
pub struct StatsAccumulator {
    colour: ZoneGrid<ColourZone>,
    luma: ZoneGrid<LumaZone>,
    bins: Vec<u64>,
    saturation: f64,
}

impl StatsAccumulator {
    /// A `width × height` zone grid and a histogram of `bins` bins. Pixels with any channel at or
    /// above `saturation` are left out of the zone sums (they still count in the histogram).
    pub fn new(width: u32, height: u32, bins: usize, saturation: f64) -> Self {
        Self {
            colour: ZoneGrid::new(width, height),
            luma: ZoneGrid::new(width, height),
            bins: vec![0; bins.max(1)],
            saturation,
        }
    }

    /// Add one normalised RGB pixel to zone `(zx, zy)`.
    pub fn add(&mut self, zx: u32, zy: u32, r: f64, g: f64, b: f64) {
        let i = (zy * self.colour.width + zx) as usize;
        let y = rec601(r, g, b);
        let n = self.bins.len();
        let bin = ((y.clamp(0.0, 1.0) * n as f64) as usize).min(n - 1);
        self.bins[bin] += 1;
        if r >= self.saturation || g >= self.saturation || b >= self.saturation {
            return;
        }
        let z = &mut self.colour.zones[i];
        z.r += r;
        z.g += g;
        z.b += b;
        z.counted += 1;
        let l = &mut self.luma.zones[i];
        l.y += y;
        l.counted += 1;
    }

    /// The statistics (taken before white balance and lens shading).
    pub fn finish(self) -> Statistics {
        Statistics {
            colour: self.colour,
            luma: Some(self.luma),
            histogram: Histogram::from(self.bins),
            focus: None,
            before_wb: true,
            before_lsc: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_quantiles() {
        let h = Histogram::from(vec![10, 10, 10, 10]);
        assert_eq!(h.total(), 40);
        assert_eq!(h.quantile(0.5), 2.0);
        assert_eq!(h.quantile(0.25), 1.0);
        assert!((h.inter_quantile_mean(0.0, 1.0) - 2.0).abs() < 1e-12);
        // Top 25%: all in the last bin, mean at its centre.
        assert!((h.inter_quantile_mean(0.75, 1.0) - 3.5).abs() < 1e-12);
        assert_eq!(h.cumulative_freq(1.5), 15.0);
    }

    #[test]
    fn accumulator_skips_saturated_pixels_in_zones() {
        let mut a = StatsAccumulator::new(2, 1, 8, 0.99);
        a.add(0, 0, 0.5, 0.5, 0.5);
        a.add(1, 0, 1.0, 1.0, 1.0);
        let s = a.finish();
        assert_eq!(s.colour.zones[0].counted, 1);
        assert_eq!(s.colour.zones[1].counted, 0);
        assert_eq!(s.histogram.total(), 2);
        assert!((s.mean_luma() - 0.5).abs() < 1e-12);
    }

    #[test]
    fn statistics_round_trip_json() {
        let mut a = StatsAccumulator::new(2, 2, 4, 1.0);
        a.add(1, 1, 0.1, 0.2, 0.3);
        let s = a.finish();
        let j = serde_json::to_string(&s).unwrap();
        let back: Statistics = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
    }
}
