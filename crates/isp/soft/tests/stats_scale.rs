//! The 3A statistics the stage hands the controller are on its scale: pixel values 0..1 after the
//! black level, before the white balance gains. A frame of known codes gives known means.
//!
//! The board's dark auto exposure came from this: the sums were handed over on the 12-bit working
//! scale (0..4095), so a mid-grey scene read about 4000 times too bright. The sim's tests ran auto
//! exposure only on the PiSP route, whose statistics were already 0..1.

use crate::common::*;

use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, FrameLease, FramePool, packed_layouts};
use styx_soft::SoftIspStage;
use styx_soft::params::{Demosaic, YuvMatrix};

const BLACK: u16 = 64;

/// A BGGR RAW10 frame with every blue site at `b`, green at `g` and red at `r`.
fn bayer(pool: &FramePool<()>, [r, g, b]: [u16; 3]) -> FrameLease<()> {
    let layouts = packed_layouts(RAW, W, H, None, None).unwrap();
    let format = MediaFormat::with_defaults(RAW, Resolution::new(W, H).unwrap());
    let mut lease = pool
        .acquire(FrameEntry::new(1, 1_000, format, layouts))
        .unwrap();
    // Even rows B G B G, odd rows G R G R; four pixels a 5-byte group.
    let group = |a: u16, c: u16| {
        let lo = |v: u16| (v & 3) as u8;
        [
            (a >> 2) as u8,
            (c >> 2) as u8,
            (a >> 2) as u8,
            (c >> 2) as u8,
            lo(a) | lo(c) << 2 | lo(a) << 4 | lo(c) << 6,
        ]
    };
    {
        let mut planes = lease.write().unwrap();
        let mut plane = planes.plane(0).unwrap();
        for row in 0..H as usize {
            let g5 = if row % 2 == 0 {
                group(b, g)
            } else {
                group(g, r)
            };
            for (i, byte) in plane
                .row_bytes_mut(row, RAW_STRIDE)
                .unwrap()
                .iter_mut()
                .enumerate()
            {
                *byte = g5[i % 5];
            }
        }
    }
    lease
}

fn level(code: u16) -> f64 {
    f64::from(code - BLACK) / f64::from(1023 - BLACK)
}

#[test]
fn statistics_are_handed_over_on_0_to_1_before_the_gains() {
    let raw_pool = picture_pool(RAW, W, H, 1);
    let out_pool = picture_pool(FourCc::RGB3, W, H, 1);
    // A warm grey: R/G 1.25, B/G 0.4 above black. Gains that are not 1 (the sums carry them).
    let codes = [64 + 300, 64 + 240, 64 + 96];
    let raw = bayer(&raw_pool, codes);
    let mut s = styx_settings::IspSettings::neutral(f64::from(BLACK) / 1023.0);
    s.wb = [0.8, 1.0, 2.5];
    s.digital_gain = 1.2;
    let options = options(Demosaic::Bilinear, YuvMatrix::Bt709Limited, true);
    let mut stage = SoftIspStage::new(RAW, W, H, &s, options).unwrap();
    let mut out = picture(&out_pool, FourCc::RGB3, W, H, 1);
    let mut stats = styx_aaa::Statistics::default();
    stage.process(&raw, &mut out, Some(&mut stats)).unwrap();

    assert!(stats.before_wb && !stats.before_lsc);
    let want = codes.map(level);
    let counted: u32 = stats.colour.zones.iter().map(|z| z.counted).sum();
    assert!(counted > 0, "no quad counted");
    for (i, z) in stats.colour.zones.iter().enumerate() {
        let (r, g, b) = z.mean();
        for (got, want, name) in [(r, want[0], "R"), (g, want[1], "G"), (b, want[2], "B")] {
            assert!(
                (got - want).abs() < 2e-3,
                "zone {i} {name}: {got:.5}, the frame's {want:.5} (0..1 above black, before gains)"
            );
        }
    }
    // The histogram, before the gains too: every quad of the flat frame in the bin of its quad
    // luma (R + 2G + B) / 4 above black, on 0..1 (within a bin either side, from the 10-bit to
    // 12-bit step and the bins' edges).
    let bins = stats.histogram.len();
    let quad = (want[0] + 2.0 * want[1] + want[2]) / 4.0;
    let at = (quad * bins as f64) as usize;
    let near: u64 = stats.histogram.bins()[at.saturating_sub(1)..(at + 2).min(bins)]
        .iter()
        .sum();
    assert_eq!(
        near,
        stats.histogram.total(),
        "histogram: every quad near bin {at} of {bins}, the frame's quad luma {quad:.4}: {:?}",
        stats
            .histogram
            .bins()
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .collect::<Vec<_>>()
    );
    let luma = styx_aaa::stats::rec601(want[0], want[1], want[2]);
    let got = stats.mean_luma();
    assert!(
        (got - luma).abs() < 2e-3,
        "mean luma {got:.5}, the frame's {luma:.5}"
    );
}
