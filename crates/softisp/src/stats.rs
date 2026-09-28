//! 3A statistics, gathered on 2x2 quads of the mosaic in the processing pass.

use serde::{Deserialize, Serialize};

use crate::prepare::StatsSetup;
use crate::simd::{self, scalar::WORK_MAX};

/// One zone. Sums are of quad values in the 12-bit working range (0..=4095) after black level,
/// white balance / digital gain and lens shading; divide by [`IspStats::gains`] for values
/// before the gains. The colour sums leave out quads with a clipped channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ZoneStats {
    pub r_sum: u64,
    pub g_sum: u64,
    pub b_sum: u64,
    /// Quads in the colour sums.
    pub count: u32,
    /// Mean quad luma `(R + 2G + B) / 4` over all quads of the zone, 0..1.
    pub luma: f32,
}

impl ZoneStats {
    /// Mean R, G, B of the unclipped quads, 0..1 (`None` when every quad clipped).
    pub fn mean_rgb(&self) -> Option<[f32; 3]> {
        (self.count > 0).then(|| {
            let n = self.count as f32 * WORK_MAX as f32;
            [
                self.r_sum as f32 / n,
                self.g_sum as f32 / n,
                self.b_sum as f32 / n,
            ]
        })
    }
}

/// Statistics of one frame.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IspStats {
    pub zones_x: u32,
    pub zones_y: u32,
    /// Row-major, `zones_x * zones_y`.
    pub zones: Vec<ZoneStats>,
    /// Quad luma histogram (bins evenly over 0..=4095).
    pub histogram: Vec<u32>,
    /// Quads sampled.
    pub samples: u32,
    /// The R, G, B gains (white balance times digital gain) the sums include.
    pub gains: [f32; 3],
}

impl IspStats {
    pub fn zone(&self, x: u32, y: u32) -> &ZoneStats {
        &self.zones[(y * self.zones_x + x) as usize]
    }

    /// Mean luma over the frame, 0..1.
    pub fn mean_luma(&self) -> f32 {
        let total: u64 = self.histogram.iter().map(|&c| c as u64).sum();
        if total == 0 {
            return 0.0;
        }
        let bins = self.histogram.len() as f32;
        let weighted: f64 = self
            .histogram
            .iter()
            .enumerate()
            .map(|(i, &c)| (i as f64 + 0.5) * c as f64)
            .sum();
        (weighted / total as f64) as f32 / bins
    }

    /// Grey-world white balance: gains (relative to green, on top of the gains already
    /// applied) making the mean of the unclipped quads grey. `None` without usable quads.
    pub fn grey_world_gains(&self) -> Option<[f32; 3]> {
        let (r, g, b) = self.zones.iter().fold((0u64, 0u64, 0u64), |(r, g, b), z| {
            (r + z.r_sum, g + z.g_sum, b + z.b_sum)
        });
        if r == 0 || g == 0 || b == 0 {
            return None;
        }
        Some([g as f32 / r as f32, 1.0, g as f32 / b as f32])
    }
}

/// Per-worker accumulation, merged at the end of the frame.
#[derive(Clone, Debug)]
pub(crate) struct StatsAccum {
    rgb: Vec<[u64; 3]>,
    count: Vec<u32>,
    luma: Vec<u64>,
    quads: Vec<u32>,
    /// Four interleaved histograms (quad `i` counts in copy `i % 4`), so runs of one bin do not
    /// serialise on a single counter; summed at the end.
    pub histogram: Vec<u32>,
    bins: usize,
    samples: u32,
}

impl StatsAccum {
    pub fn new(setup: &StatsSetup) -> Self {
        let zones = (setup.config.zones_x * setup.config.zones_y) as usize;
        Self {
            rgb: vec![[0; 3]; zones],
            count: vec![0; zones],
            luma: vec![0; zones],
            quads: vec![0; zones],
            histogram: vec![0; 4 * setup.config.histogram_bins as usize],
            bins: setup.config.histogram_bins as usize,
            samples: 0,
        }
    }

    pub fn reset(&mut self) {
        self.rgb.fill([0; 3]);
        self.count.fill(0);
        self.luma.fill(0);
        self.quads.fill(0);
        self.histogram.fill(0);
        self.samples = 0;
    }

    /// Add quad row `qy` (R, G, B of each quad).
    pub fn add_row(&mut self, setup: &StatsSetup, qy: usize, rgb: [&[u16]; 3]) {
        let zy = qy * setup.config.zones_y as usize / setup.quad_rows;
        let zx_count = setup.config.zones_x as usize;
        for zx in 0..zx_count {
            let (c0, c1) = (setup.col_edges[zx], setup.col_edges[zx + 1]);
            let seg = [&rgb[0][c0..c1], &rgb[1][c0..c1], &rgb[2][c0..c1]];
            let ([rs, gs, bs, n, ls], _) = simd::zone_sums(seg, c1 - c0, setup.saturation);
            let z = zy * zx_count + zx;
            let acc = &mut self.rgb[z];
            acc[0] += rs as u64;
            acc[1] += gs as u64;
            acc[2] += bs as u64;
            self.count[z] += n;
            self.luma[z] += ls as u64;
            self.quads[z] += (c1 - c0) as u32;
        }
        // Histogram: four quads at a time into the four copies.
        let bins = self.bins;
        let bin = |r: u16, g: u16, b: u16| {
            let y = (r as u32 + 2 * g as u32 + b as u32 + 2) >> 2;
            ((y * bins as u32) >> 12) as usize
        };
        let (h0, rest) = self.histogram.split_at_mut(bins);
        let (h1, rest) = rest.split_at_mut(bins);
        let (h2, h3) = rest.split_at_mut(bins);
        let (r, g, b) = (rgb[0], rgb[1], rgb[2]);
        let quads = r.len().min(g.len()).min(b.len());
        let mut i = 0;
        while i + 4 <= quads {
            h0[bin(r[i], g[i], b[i])] += 1;
            h1[bin(r[i + 1], g[i + 1], b[i + 1])] += 1;
            h2[bin(r[i + 2], g[i + 2], b[i + 2])] += 1;
            h3[bin(r[i + 3], g[i + 3], b[i + 3])] += 1;
            i += 4;
        }
        for k in i..quads {
            h0[bin(r[k], g[k], b[k])] += 1;
        }
        self.samples += quads as u32;
    }

    pub fn merge(&mut self, other: &Self) {
        for (a, b) in self.rgb.iter_mut().zip(&other.rgb) {
            for k in 0..3 {
                a[k] += b[k];
            }
        }
        let add = |a: &mut [u32], b: &[u32]| a.iter_mut().zip(b).for_each(|(a, b)| *a += b);
        add(&mut self.count, &other.count);
        add(&mut self.quads, &other.quads);
        add(&mut self.histogram, &other.histogram);
        self.luma
            .iter_mut()
            .zip(&other.luma)
            .for_each(|(a, b)| *a += b);
        self.samples += other.samples;
    }

    pub fn finish(&self, setup: &StatsSetup, gains: [f32; 3]) -> IspStats {
        let zones = (0..self.rgb.len())
            .map(|z| ZoneStats {
                r_sum: self.rgb[z][0],
                g_sum: self.rgb[z][1],
                b_sum: self.rgb[z][2],
                count: self.count[z],
                luma: if self.quads[z] == 0 {
                    0.0
                } else {
                    (self.luma[z] as f64 / (self.quads[z] as f64 * WORK_MAX as f64)) as f32
                },
            })
            .collect();
        IspStats {
            zones_x: setup.config.zones_x,
            zones_y: setup.config.zones_y,
            zones,
            histogram: (0..self.bins)
                .map(|b| (0..4).map(|c| self.histogram[c * self.bins + b]).sum())
                .collect(),
            samples: self.samples,
            gains,
        }
    }
}
