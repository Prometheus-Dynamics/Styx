//! Statistics from raw Bayer rows, for a frame path whose ISP gives none: rows are handed over as
//! a pass that already reads them goes (a copy into an ISP's input, a statistics pass of its own),
//! and every `row_step`-th quad row and `col_step`-th quad of it is counted.
//!
//! Each counted 2x2 quad gives one R, the mean of its two Gs and one B, black level removed and
//! normalised so full scale is `1.0` (see [`crate::stats`]). A quad with any sample at or above the
//! saturation level is left out of the colour zones and still counted in the luma zones and the
//! histogram, as [`crate::StatsAccumulator`] does. The sums are kept in integers in the samples'
//! own units and scaled once, in [`BayerStats::finish_into`].
//!
//! Every buffer is sized when the accumulator is made: a frame's rows, its reset and its
//! [`finish_into`](BayerStats::finish_into) a [`Statistics`] whose buffers have grown allocate
//! nothing.

use alloc::{vec, vec::Vec};

use crate::stats::{ColourZone, LumaZone, Statistics, ZoneGrid};

/// The Bayer order of the rows: the colours of the top-left quad, row by row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cfa {
    /// R G / G B.
    Rggb,
    /// G R / B G.
    Grbg,
    /// G B / R G.
    Gbrg,
    /// B G / G R.
    Bggr,
}

/// Channel indices: 0 red, 1 green, 2 blue.
const R: u8 = 0;
const G: u8 = 1;
const B: u8 = 2;

impl Cfa {
    /// The channel at each position of a quad: `[top left, top right, bottom left, bottom right]`.
    fn channels(self) -> [u8; 4] {
        match self {
            Cfa::Rggb => [R, G, G, B],
            Cfa::Grbg => [G, R, B, G],
            Cfa::Gbrg => [G, B, R, G],
            Cfa::Bggr => [B, G, G, R],
        }
    }
}

/// What a [`BayerStats`] counts and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BayerStatsConfig {
    /// Frame width and height, in pixels.
    pub width: u32,
    pub height: u32,
    /// The Bayer order.
    pub cfa: Cfa,
    /// Significant bits of the samples handed over (8, 10, 12 or 16), in the low bits.
    pub bits: u8,
    /// Colour and luma zones across and down.
    pub zones: (u32, u32),
    /// Histogram bins over normalised luma.
    pub bins: u32,
    /// Count every `row_step`-th quad row (1: every one).
    pub row_step: u32,
    /// Count every `col_step`-th quad of a counted row (1: every one).
    pub col_step: u32,
}

impl BayerStatsConfig {
    /// The back end's colour grid (32x32), a 128-bin histogram, and every other quad of every
    /// other quad row (a sixteenth of the pixels).
    pub fn new(width: u32, height: u32, cfa: Cfa, bits: u8) -> Self {
        Self {
            width,
            height,
            cfa,
            bits,
            zones: (32, 32),
            bins: 128,
            row_step: 2,
            col_step: 2,
        }
    }
}

/// Rec.601 luma weights of R, mean G and B, in 1/1024.
const LUMA_R: u64 = 306;
const LUMA_G: u64 = 601;
const LUMA_B: u64 = 117;

/// The accumulator. See the [module documentation](self).
#[derive(Debug, Clone)]
pub struct BayerStats {
    config: BayerStatsConfig,
    channels: [u8; 4],
    quad_cols: usize,
    quad_rows: usize,
    /// The zone column of each counted quad (indexed by counted quad, in order).
    zone_col: Vec<u16>,
    /// Black level and saturation, in sample units.
    black: u32,
    saturation: u32,
    max: u32,
    /// Per zone: R, G (both greens) and B sums of unsaturated quads, black removed.
    rgb: Vec<[u64; 3]>,
    count: Vec<u32>,
    /// Per zone: luma sums (in 1/1024 of sample units, black removed) and quads, of every quad.
    luma: Vec<u64>,
    quads: Vec<u32>,
    histogram: Vec<u64>,
    /// The upper row of the quad row being read, and its frame row.
    upper: Vec<u16>,
    upper_y: Option<usize>,
}

/// The sums of a run of quads in one zone.
#[derive(Default)]
struct Run {
    rgb: [u64; 3],
    count: u32,
    luma: u64,
    quads: u32,
}

impl BayerStats {
    /// An accumulator for frames of `config`, with black level `black` in sample units.
    pub fn new(config: BayerStatsConfig, black: u32) -> Self {
        let mut config = config;
        config.zones.0 = config.zones.0.max(1);
        config.zones.1 = config.zones.1.max(1);
        config.bins = config.bins.max(1);
        config.row_step = config.row_step.max(1);
        config.col_step = config.col_step.max(1);
        config.bits = config.bits.clamp(1, 16);
        let quad_cols = (config.width / 2) as usize;
        let quad_rows = ((config.height / 2) as usize).max(1);
        let zones = (config.zones.0 * config.zones.1) as usize;
        let zone_col = (0..quad_cols)
            .step_by(config.col_step as usize)
            .map(|qx| (qx * config.zones.0 as usize / quad_cols.max(1)) as u16)
            .collect();
        let max = (1u32 << config.bits) - 1;
        let mut stats = Self {
            config,
            channels: config.cfa.channels(),
            quad_cols,
            quad_rows,
            zone_col,
            black: 0,
            saturation: max,
            max,
            rgb: vec![[0; 3]; zones],
            count: vec![0; zones],
            luma: vec![0; zones],
            quads: vec![0; zones],
            histogram: vec![0; config.bins as usize],
            upper: vec![0; config.width as usize],
            upper_y: None,
        };
        stats.set_black(black);
        stats
    }

    /// The configuration.
    pub fn config(&self) -> &BayerStatsConfig {
        &self.config
    }

    /// Sets the black level, in sample units, from the next row on. Saturation is 1/64 below
    /// full scale.
    pub fn set_black(&mut self, black: u32) {
        self.black = black.min(self.max.saturating_sub(1));
        self.saturation = self.max - (self.max >> 6);
    }

    /// Whether frame row `y` is in a counted quad row (the caller may skip handing it over).
    pub fn wants(&self, y: usize) -> bool {
        (y / 2).is_multiple_of(self.config.row_step as usize)
    }

    /// Clears the sums for a new frame.
    pub fn reset(&mut self) {
        self.rgb.fill([0; 3]);
        self.count.fill(0);
        self.luma.fill(0);
        self.quads.fill(0);
        self.histogram.fill(0);
        self.upper_y = None;
    }

    /// Hands over frame row `y`, its samples in the low `bits` bits. A row outside a counted quad
    /// row, or a lower row without its upper one, is ignored.
    pub fn add_row(&mut self, y: usize, row: &[u16]) {
        if !self.wants(y) || y / 2 >= self.quad_rows {
            return;
        }
        let width = (self.config.width as usize).min(row.len());
        if y.is_multiple_of(2) {
            self.upper[..width].copy_from_slice(&row[..width]);
            self.upper_y = Some(y);
            return;
        }
        if self.upper_y != Some(y - 1) {
            return;
        }
        self.upper_y = None;
        let zy = (y / 2) * self.config.zones.1 as usize / self.quad_rows;
        let zone_row = zy * self.config.zones.0 as usize;
        let step = self.config.col_step as usize;
        let (black, saturation) = (self.black, self.saturation);
        let span = u64::from(self.max - black).max(1);
        let bins = self.histogram.len() as u64;
        // The histogram bin by a multiply and a shift, not a divide per quad: `y_sum * bins /
        // (1024 * span)`, one bin lower at most on a bin's edge. `y_sum` is at most 1024 * span
        // (26 bits at 16-bit samples) and `scale` at most 2^29, so the product fits.
        let scale = (bins << 32) / (1024 * span);
        let ch = self.channels;
        let last = width.saturating_sub(1);
        // A run of quads in one zone is summed here and added to the zone once.
        let mut run = Run::default();
        let mut zone = usize::MAX;
        for (i, qx) in (0..self.quad_cols).step_by(step).enumerate() {
            let x = 2 * qx;
            if x + 1 > last {
                break;
            }
            let z = zone_row + self.zone_col[i] as usize;
            if z != zone {
                self.flush(zone, &run);
                (zone, run) = (z, Run::default());
            }
            let s = [
                u32::from(self.upper[x]),
                u32::from(self.upper[x + 1]),
                u32::from(row[x]),
                u32::from(row[x + 1]),
            ];
            let mut c = [0u64; 3];
            let mut clipped = false;
            for (v, k) in s.iter().zip(ch) {
                clipped |= *v >= saturation;
                c[k as usize] += u64::from(v.saturating_sub(black));
            }
            // c[1] holds both greens.
            let y_sum = LUMA_R * c[0] + LUMA_G * c[1] / 2 + LUMA_B * c[2];
            run.luma += y_sum;
            run.quads += 1;
            let bin = ((y_sum * scale) >> 32).min(bins - 1);
            self.histogram[bin as usize] += 1;
            if !clipped {
                run.rgb[0] += c[0];
                run.rgb[1] += c[1];
                run.rgb[2] += c[2];
                run.count += 1;
            }
        }
        self.flush(zone, &run);
    }

    /// Adds a run of quads' sums to zone `z` (`usize::MAX`: none yet).
    fn flush(&mut self, z: usize, run: &Run) {
        if z == usize::MAX || run.quads == 0 {
            return;
        }
        let acc = &mut self.rgb[z];
        acc[0] += run.rgb[0];
        acc[1] += run.rgb[1];
        acc[2] += run.rgb[2];
        self.count[z] += run.count;
        self.luma[z] += run.luma;
        self.quads[z] += run.quads;
    }

    /// Quads counted in the frame so far (each once, saturated or not).
    pub fn quads(&self) -> u64 {
        self.quads.iter().map(|&q| u64::from(q)).sum()
    }

    /// The frame's statistics, written into `out` (its buffers reused): colour and luma zones,
    /// the histogram, no focus figures, taken before white balance and lens shading.
    pub fn finish_into(&self, out: &mut Statistics) {
        let (w, h) = self.config.zones;
        let span = f64::from(self.max - self.black).max(1.0);
        out.colour.width = w;
        out.colour.height = h;
        out.colour.zones.clear();
        out.colour
            .zones
            .extend(self.rgb.iter().zip(&self.count).map(|(s, &n)| ColourZone {
                r: s[0] as f64 / span,
                g: s[1] as f64 / (2.0 * span),
                b: s[2] as f64 / span,
                counted: n,
            }));
        let luma = out.luma.get_or_insert_with(|| ZoneGrid::new(0, 0));
        luma.width = w;
        luma.height = h;
        luma.zones.clear();
        luma.zones
            .extend(self.luma.iter().zip(&self.quads).map(|(&y, &n)| LumaZone {
                y: y as f64 / (1024.0 * span),
                counted: n,
            }));
        out.histogram.set_bins(self.histogram.iter().copied());
        out.focus = None;
        out.pdaf = None;
        out.before_wb = true;
        out.before_lsc = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat frame: every R, G and B sample at its own value.
    fn flat(config: BayerStatsConfig, black: u32, rgb: [u16; 3]) -> Statistics {
        let ch = config.cfa.channels();
        let mut stats = BayerStats::new(config, black);
        let w = config.width as usize;
        let mut row = vec![0u16; w];
        for y in 0..config.height as usize {
            for (x, v) in row.iter_mut().enumerate() {
                *v = rgb[ch[(y % 2) * 2 + x % 2] as usize];
            }
            if stats.wants(y) {
                stats.add_row(y, &row);
            }
        }
        let mut out = Statistics::new(ZoneGrid::new(0, 0), None, Default::default(), None);
        stats.finish_into(&mut out);
        out
    }

    #[test]
    fn a_flat_frame_gives_its_levels_less_black_in_every_zone_for_every_order() {
        for cfa in [Cfa::Rggb, Cfa::Grbg, Cfa::Gbrg, Cfa::Bggr] {
            let mut config = BayerStatsConfig::new(64, 48, cfa, 10);
            config.zones = (4, 3);
            let s = flat(config, 64, [64 + 480, 64 + 240, 64 + 96]);
            assert_eq!((s.colour.width, s.colour.height), (4, 3));
            for z in &s.colour.zones {
                assert!(z.counted > 0, "{cfa:?}");
                let (r, g, b) = z.mean();
                let span = 1023.0 - 64.0;
                assert!((r - 480.0 / span).abs() < 1e-9, "{cfa:?} r {r}");
                assert!((g - 240.0 / span).abs() < 1e-9, "{cfa:?} g {g}");
                assert!((b - 96.0 / span).abs() < 1e-9, "{cfa:?} b {b}");
            }
            let y = crate::stats::rec601(480.0, 240.0, 96.0) / (1023.0 - 64.0);
            assert!(
                (s.mean_luma() - y).abs() < 2e-3,
                "{cfa:?} {}",
                s.mean_luma()
            );
        }
    }

    #[test]
    fn a_sixteenth_of_the_quads_is_counted_by_default() {
        let config = BayerStatsConfig::new(64, 64, Cfa::Rggb, 10);
        let s = flat(config, 0, [100, 100, 100]);
        // 32x32 quads; every other quad of every other quad row.
        assert_eq!(s.histogram.total(), 16 * 16);
    }

    #[test]
    fn saturated_quads_leave_the_colour_zones_and_stay_in_luma_and_the_histogram() {
        let mut config = BayerStatsConfig::new(8, 8, Cfa::Rggb, 10);
        config.zones = (1, 1);
        (config.row_step, config.col_step) = (1, 1);
        let s = flat(config, 0, [1023, 100, 100]);
        assert_eq!(s.colour.zones[0].counted, 0);
        assert_eq!(s.luma.as_ref().expect("luma").zones[0].counted, 16);
        assert_eq!(s.histogram.total(), 16);
    }

    #[test]
    fn a_lower_row_without_its_upper_row_counts_nothing_and_reset_clears() {
        let mut config = BayerStatsConfig::new(8, 4, Cfa::Rggb, 8);
        config.zones = (1, 1);
        (config.row_step, config.col_step) = (1, 1);
        let mut stats = BayerStats::new(config, 0);
        stats.add_row(1, &[10; 8]);
        assert_eq!(stats.quads(), 0);
        stats.add_row(0, &[10; 8]);
        stats.add_row(1, &[10; 8]);
        assert_eq!(stats.quads(), 4);
        stats.reset();
        assert_eq!(stats.quads(), 0);
    }

    #[test]
    fn finishing_into_grown_statistics_reuses_their_buffers() {
        let config = BayerStatsConfig::new(64, 64, Cfa::Rggb, 10);
        let mut stats = BayerStats::new(config, 0);
        let mut out = Statistics::new(ZoneGrid::new(0, 0), None, Default::default(), None);
        stats.finish_into(&mut out);
        let ptr = out.colour.zones.as_ptr();
        stats.reset();
        stats.add_row(0, &[5; 64]);
        stats.add_row(1, &[5; 64]);
        stats.finish_into(&mut out);
        assert_eq!(out.colour.zones.as_ptr(), ptr);
    }
}
