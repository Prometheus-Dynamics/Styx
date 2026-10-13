//! Tuning overlays and per-channel black levels: a Styx overlay merges over a Raspberry Pi
//! tuning (deep, per algorithm block), and the Gr and Gb levels and the per-gain lists it can
//! carry reach the algorithms.

use styx_algo::Tuning;
use styx_algo::tuning::{BlackLevelTuning, NoiseTuning};

const FIXTURE: &str = include_str!("../data/rpi-minimal.json");

#[test]
fn an_empty_overlay_changes_nothing() {
    let base = Tuning::from_rpi_json_str(FIXTURE).unwrap().tuning;
    let merged = Tuning::from_rpi_json_str_with_overlay(FIXTURE, r#"{"overlay_version": 1}"#)
        .unwrap()
        .tuning;
    assert_eq!(merged, base);
}

#[test]
fn overlay_blocks_deep_merge_into_the_tuning_and_leave_the_rest_alone() {
    let base = Tuning::from_rpi_json_str(FIXTURE).unwrap().tuning;
    let overlay = r#"{
        "sensor": "fixture",
        "algorithms": [
            { "rpi.black_level": { "black_level_gr": 4100, "black_level_gb": 4104 } },
            { "rpi.noise": { "reference_constant": 12.5 } }
        ]
    }"#;
    let t = Tuning::from_rpi_json_str_with_overlay(FIXTURE, overlay)
        .unwrap()
        .tuning;
    // The overlay's keys win; the file's other keys stay (black_level's 4096 is still there).
    let b = t.black_level.clone().unwrap();
    assert_eq!(b.g, 4096.0 / 65536.0);
    assert_eq!(b.gr, Some(4100.0 / 65536.0));
    assert_eq!(b.gb, Some(4104.0 / 65536.0));
    let n = &t.denoise.as_ref().unwrap().noise;
    assert_eq!(n.reference_constant, 12.5);
    assert_eq!(n.reference_slope, 3.0, "the file's slope is kept");
    // Blocks the overlay does not name are the file's.
    assert_eq!(t.agc, base.agc);
    assert_eq!(t.awb, base.awb);
}

#[test]
fn per_gain_levels_and_noise_come_from_the_overlay() {
    let overlay = r#"{
        "algorithms": [
            { "rpi.black_level": { "black_level": 4096,
                "by_gain": [
                    { "gain": 1.0, "black_level_gr": 4096 },
                    { "gain": 4.0, "black_level": 4480, "black_level_gb": 4500 }
                ] } },
            { "rpi.noise": { "reference_constant": 128,
                "by_gain": [ { "gain": 1.0, "reference_constant": 128 },
                             { "gain": 4.0, "reference_constant": 160 } ] } }
        ]
    }"#;
    let t = Tuning::from_rpi_json_str_with_overlay(FIXTURE, overlay)
        .unwrap()
        .tuning;
    let b = t.black_level.unwrap();
    assert_eq!(b.by_gain.len(), 2);
    // At gain 4 every channel is 4480 except Gb (4500); Gr falls back to the entry's level.
    let at4 = b.at_gain(4.0);
    assert_eq!(
        at4.cfa(),
        [
            4480.0 / 65536.0,
            4480.0 / 65536.0,
            4500.0 / 65536.0,
            4480.0 / 65536.0
        ]
    );
    // Between the entries Gr is interpolated, and split because the tuning splits green.
    // Gr: 4096 at gain 1, 4480 at gain 4 (the entry's level); at gain 2, a third of the way.
    let at2 = b.at_gain(2.0);
    assert!((at2.gr.unwrap() - 4224.0 / 65536.0).abs() < 1e-12);
    let n = t.denoise.unwrap().noise;
    assert_eq!(n.constant_at(1.0), 128.0);
    assert!((n.constant_at(2.0) - 128.0 - 32.0 / 3.0).abs() < 1e-9);
    assert_eq!(n.constant_at(9.0), 160.0);
}

#[test]
fn a_file_without_split_greens_gives_none_for_gr_and_gb() {
    let b = Tuning::from_rpi_json_str(FIXTURE)
        .unwrap()
        .tuning
        .black_level
        .unwrap();
    assert!(!b.splits_green());
    let at = b.at_gain(1.0);
    assert_eq!(at.gr, None);
    assert_eq!(at.gb, None);
    assert_eq!(at.cfa(), [at.r, at.g, at.g, at.b]);
    assert_eq!(BlackLevelTuning::uniform(0.06).at_gain(3.0).gb, None);
    assert_eq!(NoiseTuning::default().constant_at(5.0), 0.0);
}

#[test]
fn an_overlay_that_does_not_validate_is_refused() {
    let bad = r#"{ "algorithms": [ { "rpi.black_level": { "black_level": 70000 } } ] }"#;
    assert!(Tuning::from_rpi_json_str_with_overlay(FIXTURE, bad).is_err());
    let not_list = r#"{ "algorithms": 3 }"#;
    assert!(Tuning::from_rpi_json_str_with_overlay(FIXTURE, not_list).is_err());
    let unsorted = r#"{ "algorithms": [ { "rpi.noise": { "by_gain": [
        { "gain": 4.0, "reference_constant": 1 }, { "gain": 1.0, "reference_constant": 1 } ] } } ] }"#;
    assert!(Tuning::from_rpi_json_str_with_overlay(FIXTURE, unsorted).is_err());
}
