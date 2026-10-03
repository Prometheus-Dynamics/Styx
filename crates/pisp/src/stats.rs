//! Front end statistics decoded into plain Rust values.

use crate::uapi::*;

/// One white balance zone: channel sums over the pixels counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AwbZone {
    /// Sum of R.
    pub r_sum: u32,
    /// Sum of G.
    pub g_sum: u32,
    /// Sum of B.
    pub b_sum: u32,
    /// Pixels (Bayer quads) counted.
    pub counted: u32,
}

impl AwbZone {
    /// Mean `(r, g, b)` per counted pixel, `None` when nothing was counted.
    pub fn mean(&self) -> Option<(f64, f64, f64)> {
        (self.counted > 0).then(|| {
            let n = f64::from(self.counted);
            (
                f64::from(self.r_sum) / n,
                f64::from(self.g_sum) / n,
                f64::from(self.b_sum) / n,
            )
        })
    }
}

/// One luminance zone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgcZone {
    /// Sum of Y.
    pub y_sum: u64,
    /// Pixels counted.
    pub counted: u32,
}

impl AgcZone {
    /// Mean Y per counted pixel.
    pub fn mean(&self) -> Option<f64> {
        (self.counted > 0).then(|| self.y_sum as f64 / f64::from(self.counted))
    }
}

/// Statistics for one frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Statistics {
    /// 32x32 white balance zones, row-major.
    pub awb_zones: Vec<AwbZone>,
    /// White balance floating regions.
    pub awb_floating: [AwbZone; FLOATING_STATS_NUM_ZONES],
    /// 1024-bin weighted luminance histogram.
    pub histogram: Vec<u32>,
    /// Row sums of Y.
    pub row_sums: Vec<u32>,
    /// Luminance floating regions.
    pub agc_floating: [AgcZone; FLOATING_STATS_NUM_ZONES],
    /// 8x8 focus figures of merit, row-major.
    pub focus: Vec<u64>,
    /// Focus floating regions.
    pub focus_floating: [u64; FLOATING_STATS_NUM_ZONES],
}

/// Error for a statistics buffer of the wrong size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadStatsLength(pub usize);

impl std::fmt::Display for BadStatsLength {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "statistics buffer is {} bytes, expected at least {}",
            self.0,
            size_of::<RawStatistics>()
        )
    }
}

impl std::error::Error for BadStatsLength {}

fn awb(z: &RawAwbZone) -> AwbZone {
    AwbZone {
        r_sum: z.r_sum,
        g_sum: z.g_sum,
        b_sum: z.b_sum,
        counted: z.counted,
    }
}

impl Statistics {
    /// Decodes a statistics buffer (as dequeued from `rp1-cfe-fe_stats`).
    pub fn parse(bytes: &[u8]) -> Result<Self, BadStatsLength> {
        let n = size_of::<RawStatistics>();
        if bytes.len() < n {
            return Err(BadStatsLength(bytes.len()));
        }
        let raw: RawStatistics = bytemuck::pod_read_unaligned(&bytes[..n]);
        Ok(Self::from_raw(&raw))
    }

    /// Converts the raw layout.
    pub fn from_raw(raw: &RawStatistics) -> Self {
        Self {
            awb_zones: raw.awb.zones.iter().map(awb).collect(),
            awb_floating: raw.awb.floating.map(|z| awb(&z)),
            histogram: raw.agc.histogram.to_vec(),
            row_sums: raw.agc.row_sums.to_vec(),
            agc_floating: raw.agc.floating.map(|z| AgcZone {
                y_sum: z.y_sum,
                counted: z.counted,
            }),
            focus: raw.cdaf.foms.to_vec(),
            focus_floating: raw.cdaf.floating,
        }
    }

    /// Totals over all AWB zones.
    pub fn awb_total(&self) -> AwbZone {
        self.awb_zones
            .iter()
            .fold(AwbZone::default(), |a, z| AwbZone {
                r_sum: a.r_sum.wrapping_add(z.r_sum),
                g_sum: a.g_sum.wrapping_add(z.g_sum),
                b_sum: a.b_sum.wrapping_add(z.b_sum),
                counted: a.counted.wrapping_add(z.counted),
            })
    }

    /// Pixels in the histogram.
    pub fn histogram_count(&self) -> u64 {
        self.histogram.iter().map(|&c| u64::from(c)).sum()
    }

    /// Mean histogram bin (0..1024), `None` for an empty histogram.
    pub fn histogram_mean(&self) -> Option<f64> {
        let n = self.histogram_count();
        (n > 0).then(|| {
            let s: f64 = self
                .histogram
                .iter()
                .enumerate()
                .map(|(i, &c)| (i as f64 + 0.5) * f64::from(c))
                .sum();
            s / n as f64
        })
    }

    /// The histogram bin below which `q` (0..1) of the pixels lie.
    pub fn histogram_quantile(&self, q: f64) -> Option<usize> {
        let n = self.histogram_count();
        if n == 0 {
            return None;
        }
        let target = (q.clamp(0.0, 1.0) * n as f64).ceil() as u64;
        let mut acc = 0u64;
        for (i, &c) in self.histogram.iter().enumerate() {
            acc += u64::from(c);
            if acc >= target.max(1) {
                return Some(i);
            }
        }
        Some(self.histogram.len() - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_synthetic_buffer() {
        let mut raw: RawStatistics = bytemuck::Zeroable::zeroed();
        raw.awb.zones[33] = RawAwbZone {
            r_sum: 300,
            g_sum: 600,
            b_sum: 150,
            counted: 3,
        };
        raw.agc.histogram[10] = 4;
        raw.agc.histogram[20] = 4;
        raw.agc.floating[0] = RawAgcZone {
            y_sum: 1000,
            counted: 10,
            pad: 0,
        };
        raw.cdaf.foms[63] = 7;
        let s = Statistics::parse(bytemuck::bytes_of(&raw)).unwrap();
        assert_eq!(s.awb_zones.len(), AWB_STATS_NUM_ZONES);
        assert_eq!(s.awb_zones[33].mean(), Some((100.0, 200.0, 50.0)));
        assert_eq!(s.awb_total().counted, 3);
        assert_eq!(s.histogram_count(), 8);
        assert_eq!(s.histogram_mean(), Some(15.5));
        assert_eq!(s.histogram_quantile(0.5), Some(10));
        assert_eq!(s.histogram_quantile(0.51), Some(20));
        assert_eq!(s.agc_floating[0].mean(), Some(100.0));
        assert_eq!(s.focus[63], 7);
        assert_eq!(Statistics::parse(&[0; 10]).unwrap_err(), BadStatsLength(10));
    }

    #[test]
    fn saturated_counts_do_not_overflow() {
        // A buffer of 0xFF bytes (a garbage or uninitialised buffer): the zone counts add up
        // past u32 and wrap like the sums instead of panicking.
        let s = Statistics::parse(&[0xFF; size_of::<RawStatistics>()]).unwrap();
        assert_eq!(s.awb_total().counted, u32::MAX.wrapping_mul(1024));
        assert_eq!(s.histogram_quantile(f64::NAN), Some(0));
    }
}
