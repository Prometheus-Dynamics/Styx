//! The analysis recovers what the synthetic captures were made with: the black level per
//! channel, the read noise, the fixed pattern, row noise, defect pixels, dark current and
//! per-gain black levels; and the overlay it writes parses in Styx.

use styx_calibrate::dark::{self, DarkOptions, DefectReason};
use styx_calibrate::overlay::{self, Provenance};
use styx_calibrate::raw::Channel;
use styx_calibrate::synth::{self, Spec};

fn analyse(spec: &Spec) -> dark::DarkCapture {
    dark::analyse(&synth::capture(spec), &DarkOptions::default()).expect("analyse")
}

#[test]
fn recovers_black_per_channel_and_read_noise() {
    let mut spec = Spec::base();
    spec.black = [64.0, 66.0, 65.0, 63.0];
    let c = analyse(&spec);
    for (i, ch) in c.channels.iter().enumerate() {
        assert_eq!(ch.channel, Channel::ALL[i]);
        assert!(
            (ch.black_median - spec.black[i]).abs() < 0.3,
            "{:?} median {} vs {}",
            ch.channel,
            ch.black_median,
            spec.black[i]
        );
        assert!(
            (ch.black_mean - spec.black[i]).abs() < 0.3,
            "{:?} mean {}",
            ch.channel,
            ch.black_mean
        );
        assert!(
            (ch.read_noise - spec.read_noise).abs() / spec.read_noise < 0.05,
            "{:?} read noise {}",
            ch.channel,
            ch.read_noise
        );
        assert_eq!(ch.clipped_zero, 0.0);
    }
    assert!(
        c.defects.is_empty(),
        "no defects in a clean capture: {:?}",
        c.defects
    );
}

#[test]
fn finds_the_hot_and_stuck_pixels_and_nothing_else() {
    let mut spec = Spec::base();
    spec.hot = vec![(10, 10, 200.0), (50, 21, 100.0)];
    spec.stuck = vec![(30, 30, 70)];
    let c = analyse(&spec);
    let mut found: Vec<(usize, usize, DefectReason)> =
        c.defects.iter().map(|d| (d.x, d.y, d.reason)).collect();
    found.sort_by_key(|&(x, y, _)| (y, x));
    assert_eq!(
        found,
        vec![
            (10, 10, DefectReason::Hot),
            (50, 21, DefectReason::Hot),
            (30, 30, DefectReason::Stuck),
        ]
    );
    // The hot pixels' channels are the CFA's: (10, 10) is blue-row red? (even, even) is B.
    let hot = c.defects.iter().find(|d| d.x == 10 && d.y == 10).unwrap();
    assert_eq!(hot.channel, Channel::B);
    // Left out of the statistics, so the read noise is unchanged.
    assert!((c.channels[0].read_noise - spec.read_noise).abs() / spec.read_noise < 0.05);
}

#[test]
fn row_noise_is_separated_from_the_read_noise() {
    let clean = analyse(&Spec::base());
    let mut noisy_spec = Spec::base();
    noisy_spec.row_noise = 1.0;
    let noisy = analyse(&noisy_spec);
    let row = |c: &dark::DarkCapture| c.channels.iter().map(|ch| ch.row_noise).sum::<f64>() / 4.0;
    // A row mean of 64 samples of read noise 2 has a temporal σ of 2/√64 = 0.25.
    assert!(row(&clean) < 0.3, "clean row noise {}", row(&clean));
    assert!(
        (row(&noisy) - 1.03).abs() < 0.1,
        "row noise {}",
        row(&noisy)
    );
    // Column noise: the read noise alone gives 2/√48 = 0.29 per column (48 good rows of each
    // channel); a row offset adds only its average over the 48 rows, 1/√48, in quadrature.
    let col = |c: &dark::DarkCapture| c.channels.iter().map(|ch| ch.col_noise).sum::<f64>() / 4.0;
    assert!(
        (col(&clean) - 0.29).abs() < 0.05,
        "clean column noise {}",
        col(&clean)
    );
    assert!(
        col(&noisy) - col(&clean) < 0.1,
        "column noise {}",
        col(&noisy)
    );
}

#[test]
fn fixed_pattern_gives_dsnu_and_row_and_column_fpn() {
    let mut spec = Spec::base();
    spec.fpn = 3.0;
    let c = analyse(&spec);
    let dsnu = c.channels.iter().map(|ch| ch.dsnu).sum::<f64>() / 4.0;
    assert!((dsnu - 3.0).abs() < 0.3, "dsnu {dsnu}");
    // The spread of a row's mean over its 64 pixels of a channel: 3 / √64; a column's over
    // its 48: 3 / √48.
    let row = c.channels.iter().map(|ch| ch.row_fpn).sum::<f64>() / 4.0;
    let col = c.channels.iter().map(|ch| ch.col_fpn).sum::<f64>() / 4.0;
    assert!((row - 3.0 / 8.0).abs() < 0.08, "row fpn {row}");
    assert!((col - 3.0 / 48f64.sqrt()).abs() < 0.08, "col fpn {col}");
}

#[test]
fn dark_current_slope_and_black_by_gain_come_from_the_sweep() {
    // Black moves with gain (64 at 1×, 70 at 4×); dark current adds 200 DN/s on top.
    let mut captures = Vec::new();
    for (gain, black) in [(1.0, 64.0), (4.0, 70.0)] {
        for exposure in [8000.0, 30000.0] {
            let mut spec = Spec::base();
            spec.seed = 7 + exposure as u64 + gain as u64;
            spec.analog_gain = gain;
            spec.exposure_us = exposure;
            spec.black = [black; 4];
            spec.dark_current = 200.0;
            captures.push(analyse(&spec));
        }
    }
    let sweep = dark::sweep(captures, &DarkOptions::default()).unwrap();
    assert_eq!(sweep.gains.len(), 2);
    assert!(sweep.black_moves, "64 → 70 DN moves more than 0.5 DN");
    // The black median is the level plus the dark current over the mean exposure (19 ms).
    let mean_exposure_s = 19_000e-6;
    assert!((sweep.gains[0].black[0] - (64.0 + 200.0 * mean_exposure_s)).abs() < 0.3);
    assert!((sweep.gains[1].black[0] - (70.0 + 200.0 * mean_exposure_s)).abs() < 0.3);
    assert_eq!(sweep.dark_current.len(), 2);
    for d in &sweep.dark_current {
        assert!(
            (d.slope_per_s - 200.0).abs() < 4.0,
            "dark current {} at gain {}",
            d.slope_per_s,
            d.gain
        );
    }
    let (c, s) = sweep.read_noise_fit.expect("two gains");
    assert!(c.is_finite() && s.is_finite());
}

#[test]
fn overlay_parses_in_styx_with_the_levels_and_the_noise() {
    let mut spec = Spec::base();
    spec.black = [64.0, 66.0, 65.0, 63.0];
    let mut captures = vec![analyse(&spec)];
    let mut hot = Spec::base();
    hot.hot = vec![(40, 40, 300.0)];
    captures.push(analyse(&hot));
    let sweep = dark::sweep(captures, &DarkOptions::default()).unwrap();
    let defects: Vec<dark::DefectPixel> = sweep
        .captures
        .iter()
        .flat_map(|c| c.defects.clone())
        .collect();
    let prov = Provenance {
        sensor: "ov9782".into(),
        unit: Some("unit-7".into()),
        date: "2026-10-12".into(),
        styx_commit: "test".into(),
    };
    let doc = overlay::build(&sweep, &prov, &defects).expect("overlay");
    let text = doc.to_string();
    let t = styx_algo::Tuning::from_rpi_json_str_with_overlay(overlay::VALIDATION_BASE, &text)
        .expect("styx parses the overlay")
        .tuning;
    let b = t.black_level.expect("black level");
    // 10-bit raw × 64 is the 16-bit scale: 64 → 4096 → 0.0625.
    assert!((b.r - 64.0 / 1024.0).abs() < 1e-3, "r {}", b.r);
    assert!(
        (b.gr.unwrap() - 66.0 / 1024.0).abs() < 1e-3,
        "gr {:?}",
        b.gr
    );
    assert!(
        (b.gb.unwrap() - 65.0 / 1024.0).abs() < 1e-3,
        "gb {:?}",
        b.gb
    );
    assert!((b.b - 63.0 / 1024.0).abs() < 1e-3, "b {}", b.b);
    assert!(b.by_gain.is_empty(), "one gain: no list");
    let n = t.denoise.expect("denoise").noise;
    // The read noise (≈ 2 DN) is the constant at unity gain: 2 × 64 on the 16-bit scale.
    assert!(
        (n.reference_constant - 128.0).abs() < 8.0,
        "constant {}",
        n.reference_constant
    );
    // The defect list is in the overlay for the tools that use it.
    assert_eq!(doc["defect_pixels"][0], serde_json::json!([40, 40]));
    assert_eq!(doc["sensor"], "ov9782");
    assert_eq!(doc["unit"], "unit-7");
}
