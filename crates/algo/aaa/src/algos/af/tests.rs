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

/// Contrast of a scene whose focus peak is at `peak` dioptres, `k` times as strong (a
/// change of `k` is a scene change), read at the lens position `d`.
fn scene(d: f64, peak: f64, k: f64) -> Statistics {
    let v = k / (1.0 + ((d - peak) / 1.5).powi(2));
    stats(v)
}

const PEAK: f64 = 5.0;

/// One frame: the contrast of the scene at the lens position AF last commanded.
fn step(af: &mut Af, p: &mut Params, frame: &mut u64, c: &Controls, k: f64) {
    let d = p.af.lens_position.unwrap_or(1.0);
    af.process(&scene(d, PEAK, k), &meta(*frame, c), p);
    *frame += 1;
}

fn continuous() -> Controls {
    Controls {
        af_mode: AfMode::Continuous,
        ..Controls::default()
    }
}

fn started() -> (Af, Params, u64) {
    let mut af = Af::new(AfTuning::default()).unwrap();
    af.prepare(&config()).unwrap();
    (af, Params::default(), 0)
}

/// Runs continuous AF on the scene until a scan has focused on the peak and stopped.
fn focus_on_peak(af: &mut Af, p: &mut Params, frame: &mut u64) {
    let c = continuous();
    for _ in 0..400 {
        step(af, p, frame, &c, 1.0);
        if p.af.state == AfState::Focused {
            break;
        }
    }
    assert_eq!(p.af.state, AfState::Focused);
    assert!((p.af.lens_position.unwrap() - PEAK).abs() < 0.5);
}

#[test]
fn an_immediate_pause_holds_the_lens_and_suppresses_retriggers() {
    let (mut af, mut p, mut f) = started();
    focus_on_peak(&mut af, &mut p, &mut f);
    let held = p.af.lens_position;
    let held_code = p.lens.unwrap().position;
    let mut c = continuous();
    c.af_pause = Some(AfPause::Immediate);
    step(&mut af, &mut p, &mut f, &c, 2.0);
    assert_eq!(p.af.pause, AfPauseState::Paused);
    // The scene is now twice as strong: unpaused, continuous AF would scan again (see the
    // next test). Paused, it holds.
    c.af_pause = None;
    for _ in 0..80 {
        step(&mut af, &mut p, &mut f, &c, 2.0);
        assert_eq!(p.af.pause, AfPauseState::Paused);
        assert_ne!(p.af.state, AfState::Scanning);
        assert_eq!(p.af.lens_position, held);
        assert_eq!(p.lens.unwrap().position, held_code);
    }
}

#[test]
fn without_a_pause_the_same_scene_change_retriggers_a_scan() {
    let (mut af, mut p, mut f) = started();
    focus_on_peak(&mut af, &mut p, &mut f);
    let c = continuous();
    let mut scanned = false;
    for _ in 0..80 {
        step(&mut af, &mut p, &mut f, &c, 2.0);
        scanned |= p.af.state == AfState::Scanning;
    }
    assert!(scanned);
}

#[test]
fn an_immediate_pause_abandons_a_running_scan() {
    let (mut af, mut p, mut f) = started();
    let c = continuous();
    while p.af.state != AfState::Scanning {
        step(&mut af, &mut p, &mut f, &c, 1.0);
    }
    for _ in 0..3 {
        step(&mut af, &mut p, &mut f, &c, 1.0);
    }
    let mut c2 = c.clone();
    c2.af_pause = Some(AfPause::Immediate);
    step(&mut af, &mut p, &mut f, &c2, 1.0);
    assert_eq!(p.af.pause, AfPauseState::Paused);
    assert_eq!(p.af.state, AfState::Idle);
    let held = p.af.lens_position;
    c2.af_pause = None;
    for _ in 0..40 {
        step(&mut af, &mut p, &mut f, &c2, 1.0);
        assert_eq!(p.af.lens_position, held);
        assert_eq!(p.af.state, AfState::Idle);
    }
}

#[test]
fn a_deferred_pause_completes_the_running_scan_first() {
    let (mut af, mut p, mut f) = started();
    let c = continuous();
    focus_on_peak(&mut af, &mut p, &mut f);
    // A scene change starts a scan.
    while p.af.state != AfState::Scanning {
        step(&mut af, &mut p, &mut f, &c, 2.0);
    }
    let mut c2 = c.clone();
    c2.af_pause = Some(AfPause::Deferred);
    step(&mut af, &mut p, &mut f, &c2, 2.0);
    // The scan is still running: the pause waits for it.
    assert_eq!(p.af.pause, AfPauseState::PauseRequested);
    assert_eq!(p.af.state, AfState::Scanning);
    c2.af_pause = None;
    let mut frames = 0;
    while p.af.pause != AfPauseState::Paused {
        step(&mut af, &mut p, &mut f, &c2, 2.0);
        frames += 1;
        assert!(frames < 400, "the scan never finished");
    }
    // It finished on the peak, and holds there.
    assert_eq!(p.af.state, AfState::Focused);
    assert!((p.af.lens_position.unwrap() - PEAK).abs() < 0.5);
    let held = p.af.lens_position;
    for _ in 0..40 {
        step(&mut af, &mut p, &mut f, &c2, 2.0);
        assert_eq!(p.af.lens_position, held);
        assert_ne!(p.af.state, AfState::Scanning);
    }
}

#[test]
fn resume_continues_continuous_af() {
    let (mut af, mut p, mut f) = started();
    focus_on_peak(&mut af, &mut p, &mut f);
    let mut c = continuous();
    c.af_pause = Some(AfPause::Immediate);
    step(&mut af, &mut p, &mut f, &c, 2.0);
    assert_eq!(p.af.pause, AfPauseState::Paused);
    c.af_pause = Some(AfPause::Resume);
    step(&mut af, &mut p, &mut f, &c, 2.0);
    assert_eq!(p.af.pause, AfPauseState::Running);
    // The scene (now twice as strong) is scanned again, and the lens finds the peak.
    let mut scanned = false;
    for _ in 0..400 {
        step(&mut af, &mut p, &mut f, &c, 2.0);
        scanned |= p.af.state == AfState::Scanning;
        if scanned && p.af.state == AfState::Focused {
            break;
        }
    }
    assert!(scanned);
    assert_eq!(p.af.state, AfState::Focused);
    assert!((p.af.lens_position.unwrap() - PEAK).abs() < 0.5);
}

#[test]
fn pause_has_no_effect_in_auto_or_manual_modes() {
    // Auto: a triggered scan still runs and focuses while a pause is requested.
    let (mut af, mut p, mut f) = started();
    let mut c = Controls {
        af_mode: AfMode::Auto,
        af_pause: Some(AfPause::Immediate),
        af_trigger: 1,
        ..Controls::default()
    };
    for _ in 0..400 {
        step(&mut af, &mut p, &mut f, &c, 1.0);
        assert_eq!(p.af.pause, AfPauseState::Running);
        if p.af.state == AfState::Focused {
            break;
        }
    }
    assert_eq!(p.af.state, AfState::Focused);
    assert!((p.af.lens_position.unwrap() - PEAK).abs() < 0.5);

    // Manual: the lens position control still moves the lens.
    let (mut af, mut p, mut f) = started();
    c = Controls {
        af_pause: Some(AfPause::Deferred),
        lens_position: Some(3.0),
        ..Controls::default()
    };
    for _ in 0..4 {
        step(&mut af, &mut p, &mut f, &c, 1.0);
        assert_eq!(p.af.pause, AfPauseState::Running);
    }
    assert_eq!(p.af.lens_position, Some(3.0));
}
