//! Unit tests of AF's state machine on synthetic statistics.

use super::*;
use crate::stats::ZoneGrid;

fn config() -> CameraConfig {
    CameraConfig {
        lens: Some(LensConfig::default()),
        ..CameraConfig::default()
    }
}

fn stats(contrast: f64) -> Statistics {
    let mut s = Statistics::default();
    let mut f = ZoneGrid::<f64>::new(8, 8);
    f.zones.fill(contrast);
    s.focus = Some(f);
    s
}

fn meta(frame: u64, c: &Controls) -> FrameMetadata {
    let mut m = FrameMetadata::new(
        frame,
        core::time::Duration::from_millis(10),
        1.0,
        core::time::Duration::from_millis(33),
    );
    m.controls = c.clone();
    m
}

#[test]
fn manual_mode_follows_the_lens_position_control() {
    let mut af = Af::new(AfTuning::default()).unwrap();
    af.prepare(&config()).unwrap();
    let mut p = Params::default();
    af.initial(&mut p);
    // The default position (1 D) on a straight 0..12 D → 0..1023 map.
    let start = p.lens.unwrap();
    assert_eq!((start.frame, start.position), (0, 85));
    let c = Controls {
        lens_position: Some(6.0),
        ..Controls::default()
    };
    for f in 0..4 {
        af.process(&stats(1.0), &meta(f, &c), &mut p);
    }
    // Slew-limited (2 D per frame): there after three frames.
    assert_eq!(p.af.lens_position, Some(6.0));
    assert_eq!(p.af.state, AfState::Idle);
    assert_eq!(p.lens.unwrap().position, 512);
}

#[test]
fn trigger_and_cancel_are_counters() {
    let mut af = Af::new(AfTuning::default()).unwrap();
    af.prepare(&config()).unwrap();
    let mut p = Params::default();
    let mut c = Controls {
        af_mode: AfMode::Auto,
        ..Controls::default()
    };
    af.process(&stats(1.0), &meta(0, &c), &mut p);
    assert_eq!(p.af.state, AfState::Idle);
    c.af_trigger = 1;
    af.process(&stats(1.0), &meta(1, &c), &mut p);
    assert_eq!(p.af.state, AfState::Scanning);
    // The same counter again is not a new trigger; a cancel stops the scan.
    c.af_cancel = 1;
    af.process(&stats(1.0), &meta(2, &c), &mut p);
    assert_eq!(p.af.state, AfState::Idle);
    af.process(&stats(1.0), &meta(3, &c), &mut p);
    assert_eq!(p.af.state, AfState::Idle);
}

#[test]
fn a_flat_curve_fails_and_returns_to_the_default() {
    let mut af = Af::new(AfTuning::default()).unwrap();
    af.prepare(&config()).unwrap();
    let mut p = Params::default();
    let c = Controls {
        af_mode: AfMode::Auto,
        af_trigger: 1,
        ..Controls::default()
    };
    for f in 0..200 {
        af.process(&stats(1.0), &meta(f, &c), &mut p);
    }
    assert_eq!(p.af.state, AfState::Failed);
    assert_eq!(p.af.lens_position, Some(1.0));
}

#[test]
fn without_a_lens_nothing_happens() {
    let mut af = Af::new(AfTuning::default()).unwrap();
    af.prepare(&CameraConfig::default()).unwrap();
    let mut p = Params::default();
    af.initial(&mut p);
    af.process(&stats(1.0), &meta(0, &Controls::default()), &mut p);
    assert!(p.lens.is_none() && !p.af.active);
}

#[test]
fn generic_map_spans_the_lens_range() {
    let m = Af::lens_map(&AfTuning::default(), &LensConfig::default());
    assert_eq!(m.eval(0.0), 0.0);
    assert_eq!(m.eval(12.0), 1023.0);
    let t = AfTuning {
        map: Pwl::new(vec![(0.0, 445.0), (15.0, 925.0)]).unwrap(),
        ..AfTuning::default()
    };
    assert_eq!(Af::lens_map(&t, &LensConfig::default()).eval(0.0), 445.0);
}
