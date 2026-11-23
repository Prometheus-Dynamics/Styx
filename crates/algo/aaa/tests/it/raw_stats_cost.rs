//! The cost of AE statistics on a raw-only stream: [`BayerStats`] over a 1280x800 RAW10 frame
//! (CSI-2 packed), unpacking only the rows it counts, as a raw-only loop would have to (the PiSP
//! route counts them in the input copy it already makes, so it pays only the counting).
//!
//! Run on a board, in release, alone on the machine:
//! `cargo test --release -p styx-aaa --test it raw_stats_cost -- --ignored --nocapture`.

use std::time::Instant;

use styx_aaa::{BayerStats, BayerStatsConfig, Cfa, Statistics};

const W: usize = 1280;
const H: usize = 800;
const STRIDE: usize = W * 5 / 4;

/// One RAW10 row unpacked into `out` (four pixels in five bytes, the low bits last).
fn unpack(row: &[u8], out: &mut [u16]) {
    for (group, px) in row
        .as_chunks::<5>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0)
    {
        let lo = group[4];
        for (i, p) in px.iter_mut().enumerate() {
            *p = (u16::from(group[i]) << 2) | u16::from((lo >> (2 * i)) & 3);
        }
    }
}

/// Nanoseconds per frame, best of `runs` runs of `frames` frames.
fn per_frame(frame: &[u8], stats: &mut BayerStats, out: &mut Statistics, unpacked: bool) -> f64 {
    let (runs, frames) = (5, 60);
    let mut row = vec![0u16; W];
    let mut best = f64::MAX;
    for _ in 0..runs {
        let start = Instant::now();
        for _ in 0..frames {
            stats.reset();
            for y in 0..H {
                if !stats.wants(y) {
                    continue;
                }
                if unpacked {
                    unpack(&frame[y * STRIDE..][..STRIDE], &mut row);
                }
                stats.add_row(y, std::hint::black_box(&row));
            }
            stats.finish_into(out);
        }
        best = best.min(start.elapsed().as_nanos() as f64 / f64::from(frames));
    }
    best
}

#[test]
#[ignore = "a measurement: run in release on the board"]
fn raw_stats_cost() {
    // A frame with some texture, so no branch is always taken.
    let frame: Vec<u8> = (0..STRIDE * H)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let mut out = Statistics::default();
    for (rows, cols) in [(2, 2), (4, 4), (8, 8)] {
        let mut config = BayerStatsConfig::new(W as u32, H as u32, Cfa::Bggr, 10);
        config.row_step = rows;
        config.col_step = cols;
        let mut stats = BayerStats::new(config, 64);
        let counting = per_frame(&frame, &mut stats, &mut out, false);
        let raw_only = per_frame(&frame, &mut stats, &mut out, true);
        // At 120 fps a frame is 8.33 ms; a CPU core's share is cost / 8.33 ms, divided by N when
        // the loop meters every Nth frame.
        let share = |ns: f64| ns / 8_333_333.0 * 100.0;
        println!(
            "raw_stats_cost 1280x800 RAW10 step {rows}x{cols}: counting {:.0} us/frame ({:.2}% of a core at 120 fps); \
             raw-only with unpack {:.0} us/frame ({:.2}% every frame, {:.2}% every 4th)",
            counting / 1e3,
            share(counting),
            raw_only / 1e3,
            share(raw_only),
            share(raw_only) / 4.0,
        );
    }
}
