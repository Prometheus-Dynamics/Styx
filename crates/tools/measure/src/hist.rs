//! A fixed-size histogram over `u64` values (nanoseconds, here): log-linear buckets, 32 per power
//! of two, so a quantile is within about 3 % of the true value. Created once, with its one
//! allocation; recording a value allocates nothing.

/// Buckets: values below 64 each have their own; above, 32 per octave up to 2^63.
const BINS: usize = 64 + 58 * 32;

#[derive(Debug, Clone)]
pub struct Histogram {
    counts: Vec<u64>,
    total: u64,
    min: u64,
    max: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub fn new() -> Self {
        Self {
            counts: vec![0; BINS],
            total: 0,
            min: u64::MAX,
            max: 0,
        }
    }

    /// Records one value.
    pub fn record(&mut self, value: u64) {
        self.counts[bin_of(value)] += 1;
        self.total += 1;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }

    /// How many values were recorded.
    pub fn len(&self) -> u64 {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// The smallest value recorded (exact), or `None`.
    pub fn min(&self) -> Option<u64> {
        (self.total > 0).then_some(self.min)
    }

    /// The largest value recorded (exact), or `None`.
    pub fn max(&self) -> Option<u64> {
        (self.total > 0).then_some(self.max)
    }

    /// The value at quantile `q` (0 to 1): the middle of its bucket, clamped to the exact min and
    /// max. `None` when nothing was recorded.
    pub fn quantile(&self, q: f64) -> Option<u64> {
        if self.total == 0 {
            return None;
        }
        let rank = ((q.clamp(0.0, 1.0) * self.total as f64).ceil() as u64).max(1);
        let mut seen = 0_u64;
        for (bin, &count) in self.counts.iter().enumerate() {
            seen += count;
            if seen >= rank {
                return Some(value_of(bin).clamp(self.min, self.max));
            }
        }
        Some(self.max)
    }

    /// The median (the 0.5 quantile).
    pub fn median(&self) -> Option<u64> {
        self.quantile(0.5)
    }
}

fn bin_of(value: u64) -> usize {
    if value < 64 {
        return value as usize;
    }
    let msb = 63 - value.leading_zeros() as usize; // >= 6
    let shift = msb - 5;
    let sub = ((value >> shift) & 31) as usize;
    64 + (msb - 6) * 32 + sub
}

/// The middle of bin `bin`.
fn value_of(bin: usize) -> u64 {
    if bin < 64 {
        return bin as u64;
    }
    let octave = (bin - 64) / 32; // msb = octave + 6
    let sub = ((bin - 64) % 32) as u64;
    let shift = (octave + 1) as u32; // msb - 5
    let low = (32 + sub) << shift;
    low + (1_u64 << shift) / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_values_are_exact_and_large_ones_within_three_percent() {
        let mut h = Histogram::new();
        for v in 0..64 {
            h.record(v);
        }
        assert_eq!(h.median(), Some(31));
        let mut big = Histogram::new();
        for v in [1_000_000_u64, 8_333_333, 9_000_000_000] {
            big.record(v);
            let got = big.quantile(1.0).expect("one value");
            assert!(got.abs_diff(v) * 32 <= v, "{v} came back as {got}");
            big = Histogram::new();
        }
        assert!(Histogram::new().quantile(0.5).is_none());
    }

    #[test]
    fn bins_stay_in_range_at_the_top() {
        assert!(bin_of(u64::MAX) < BINS);
        assert!(bin_of(1 << 63) < BINS);
    }
}
