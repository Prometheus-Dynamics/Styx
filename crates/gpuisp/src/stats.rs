//! The statistics buffer the stats shader fills, read back into [`IspStats`].

use styx_softisp::{IspStats, ZoneStats};

use crate::params::StatsSetup;

/// Words per zone (`shaders/stats.comp`): R, G, B, luma sums as (low, high), count, padding.
pub(crate) const ZONE_WORDS: usize = 10;

/// Bytes of the statistics buffer for `setup`.
pub(crate) fn buffer_bytes(setup: &StatsSetup) -> usize {
    let zones = (setup.config.zones_x * setup.config.zones_y) as usize;
    4 * (zones * ZONE_WORDS + setup.config.histogram_bins as usize)
}

/// Byte offset of the histogram.
pub(crate) fn histogram_offset(setup: &StatsSetup) -> usize {
    4 * ZONE_WORDS * (setup.config.zones_x * setup.config.zones_y) as usize
}

fn word(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
}

fn wide(b: &[u8], i: usize) -> u64 {
    word(b, i) as u64 | (word(b, i + 1) as u64) << 32
}

/// The statistics in `bytes` (the start of the buffer), as `styx-softisp` reports them,
/// written into `out` (its vectors reused).
pub(crate) fn read_into(bytes: &[u8], setup: &StatsSetup, gains: [f32; 3], out: &mut IspStats) {
    let c = &setup.config;
    let n = (c.zones_x * c.zones_y) as usize;
    out.zones.clear();
    out.zones.extend((0..n).map(|z| {
        let at = z * ZONE_WORDS;
        let quads = setup.zone_quads[z];
        let luma = wide(bytes, at + 6);
        ZoneStats {
            r_sum: wide(bytes, at),
            g_sum: wide(bytes, at + 2),
            b_sum: wide(bytes, at + 4),
            count: word(bytes, at + 8),
            luma: if quads == 0 {
                0.0
            } else {
                (luma as f64 / (quads as f64 * 4095.0)) as f32
            },
        }
    }));
    let h0 = n * ZONE_WORDS;
    out.histogram.clear();
    out.histogram
        .extend((0..c.histogram_bins as usize).map(|b| word(bytes, h0 + b)));
    out.zones_x = c.zones_x;
    out.zones_y = c.zones_y;
    out.samples = setup.samples;
    out.gains = gains;
    // Focus statistics are the CPU path's only (yet).
    out.focus.clear();
}
