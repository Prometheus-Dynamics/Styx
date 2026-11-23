//! The cost of the 3A loop's statistics on the soft ISP's route: a 1280x800 RAW10 frame made into
//! NV12 with and without them, as the service runs it (32x32 zones, 128 bins, one quad row in
//! `row_step`). The statistics are counted in the processing pass, so their cost is the
//! difference; a frame the loop skips (`aaa_stats_every`) passes `None` and must cost nothing.
//!
//! Run on a board, in release, alone on the machine:
//! `cargo test --release -p styx-soft --test all stats_cost -- --ignored --nocapture`.

use std::time::Instant;

use crate::common::*;

use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, FrameLease, FramePool, packed_layouts};
use styx_soft::SoftIspStage;
use styx_soft::params::{Demosaic, StatsConfig, YuvMatrix};

const CW: u32 = 1280;
const CH: u32 = 800;

fn frame(pool: &FramePool<()>) -> FrameLease<()> {
    let layouts = packed_layouts(RAW, CW, CH, None, None).unwrap();
    let stride = layouts[0].stride;
    let format = MediaFormat::with_defaults(RAW, Resolution::new(CW, CH).unwrap());
    let mut lease = pool
        .acquire(FrameEntry::new(1, 1_000, format, layouts))
        .unwrap();
    {
        let mut planes = lease.write().unwrap();
        let mut plane = planes.plane(0).unwrap();
        for y in 0..CH as usize {
            // Some texture around mid-grey, so no branch is always taken.
            for (i, b) in plane
                .row_bytes_mut(y, stride)
                .unwrap()
                .iter_mut()
                .enumerate()
            {
                *b = 60 + ((i * 7 + y * 13) % 97) as u8;
            }
        }
    }
    lease
}

fn stage(rows: Option<u32>) -> SoftIspStage {
    let mut options = options(Demosaic::Bilinear, YuvMatrix::Bt709Limited, false);
    options.stats = rows.map(|row_step| StatsConfig {
        zones_x: 32,
        zones_y: 32,
        histogram_bins: 128,
        row_step,
        ..StatsConfig::default()
    });
    SoftIspStage::new(RAW, CW, CH, &settings(true, true), options).unwrap()
}

/// Nanoseconds per frame, best of 5 runs of 30 frames; with the statistics when `take`.
fn per_frame(
    stage: &mut SoftIspStage,
    raw: &FrameLease<()>,
    out: &FramePool<()>,
    take: bool,
) -> f64 {
    let mut stats = styx_aaa::Statistics::default();
    let mut best = f64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        for id in 0..30 {
            let mut o = picture(out, FourCc::NV12, CW, CH, id);
            let s = take.then_some(&mut stats);
            stage.process(raw, &mut o, s).unwrap();
        }
        best = best.min(start.elapsed().as_nanos() as f64 / 30.0);
    }
    best
}

#[test]
#[ignore = "a measurement: run in release on the board"]
fn stats_cost() {
    let raw_pool = picture_pool(RAW, CW, CH, 1);
    let out = picture_pool(FourCc::NV12, CW, CH, 1);
    let raw = frame(&raw_pool);
    let share = |ns: f64| ns / 8_333_333.0 * 100.0;
    let bare = per_frame(&mut stage(None), &raw, &out, false);
    println!(
        "soft stats_cost 1280x800 RAW10 -> NV12: no statistics {:.0} us/frame",
        bare / 1e3
    );
    for rows in [2, 4] {
        let mut s = stage(Some(rows));
        let skipped = per_frame(&mut s, &raw, &out, false);
        let taken = per_frame(&mut s, &raw, &out, true);
        let cost = (taken - bare).max(0.0);
        println!(
            "soft stats_cost rows 1/{rows}: skipped frame {:.0} us ({:+.0} us), taken {:.0} us: \
             statistics {:.0} us = {:.2}% of a core at 120 fps every frame, {:.2}% every 4th",
            skipped / 1e3,
            (skipped - bare) / 1e3,
            taken / 1e3,
            cost / 1e3,
            share(cost),
            share(cost) / 4.0,
        );
    }
}
