use std::time::Duration;

use super::*;
use crate::frame::Flicker;
use crate::stats::StatsAccumulator;

/// A uniform grey frame of luma `y`.
fn grey(y: f64) -> Statistics {
    let mut a = StatsAccumulator::new(4, 3, 64, 1.0);
    for zy in 0..3 {
        for zx in 0..4 {
            for _ in 0..4 {
                a.add(zx, zy, y, y, y);
            }
        }
    }
    a.finish()
}

fn prepared(tuning: AgcTuning) -> Agc {
    let mut agc = Agc::new(tuning).unwrap();
    agc.prepare(&CameraConfig::default()).unwrap();
    agc
}

fn meta(frame: u64, ms: f64, gain: f64) -> FrameMetadata {
    FrameMetadata::new(
        frame,
        Duration::from_secs_f64(ms * 1e-3),
        gain,
        Duration::from_millis(33),
    )
}

fn no_constraints() -> AgcTuning {
    let mut t = AgcTuning::default();
    t.constraint_modes.insert("normal".into(), Vec::new());
    t
}

#[test]
fn start_up_moves_straight_to_target_and_names_the_landing_frame() {
    let mut agc = prepared(no_constraints());
    let mut p = Params::default();
    agc.process(&grey(0.04), &meta(7, 10.0, 1.0), &mut p);
    let r = p.sensor.unwrap();
    assert_eq!(r.frame, 7 + 2 + 2);
    // Target ~0.16 from 0.04: four times the total exposure.
    let total = r.exposure.as_secs_f64() * r.analogue_gain * p.digital_gain;
    assert!((total / 0.040 - 1.0).abs() < 0.05, "{total}");
    // Frames still in flight hold the request.
    let before = p.sensor;
    agc.process(&grey(0.04), &meta(8, 10.0, 1.0), &mut p);
    assert_eq!(p.sensor, before);
}

#[test]
fn exposure_profile_splits_time_then_gain() {
    let agc = prepared(no_constraints());
    let m = meta(0, 1.0, 1.0);
    // 20 ms total: exposure up to the 10 ms stage, then gain to 2.
    let s = agc.divide(0.020, 0.020, (None, None), &m);
    assert!((s.exposure - 0.010).abs() < 1e-9 && (s.analogue_gain - 2.0).abs() < 1e-9);
    // 5 ms: exposure only.
    let s = agc.divide(0.005, 0.005, (None, None), &m);
    assert!((s.exposure - 0.005).abs() < 1e-9 && s.analogue_gain == 1.0);
    // Beyond the profile's last stage (66.666 ms, gain 8): digital gain, capped at 4.
    let s = agc.divide(20.0, 20.0, (None, None), &m);
    assert_eq!(s.analogue_gain, 8.0);
    assert_eq!(s.digital_gain, 4.0);
}

#[test]
fn flicker_snaps_exposure_to_whole_periods() {
    let agc = prepared(no_constraints());
    let mut m = meta(0, 1.0, 1.0);
    m.controls.flicker = Flicker::Mains60;
    let s = agc.divide(0.030, 0.030, (None, None), &m);
    let periods = s.exposure / (1.0 / 120.0);
    assert!((periods - periods.round()).abs() < 1e-3, "{}", s.exposure);
    assert!((s.exposure * s.analogue_gain - 0.030).abs() < 1e-9);
    // Shorter than a period: left alone.
    let s = agc.divide(0.004, 0.004, (None, None), &m);
    assert!((s.exposure - 0.004).abs() < 1e-9);
}

#[test]
fn damping_after_start_up() {
    let t = AgcTuning {
        startup_frames: 0,
        ..no_constraints()
    };
    let mut agc = prepared(t);
    let mut p = Params::default();
    agc.filtered = 0.010;
    // Twice too dark: target 20 ms, damped at speed 0.2 -> 12 ms (10 ms at gain 1.2).
    agc.process(&grey(0.08), &meta(0, 10.0, 1.0), &mut p);
    let r = p.sensor.unwrap();
    let total = r.exposure.as_secs_f64() * r.analogue_gain;
    assert!((total - 0.012).abs() < 5e-4, "{total}");
}

#[test]
fn upper_constraint_limits_highlights() {
    let t = AgcTuning {
        default_constraint_mode: "highlight".into(),
        ..Default::default()
    };
    let mut agc = prepared(t);
    // Mostly dark with a bright top 5%: the mean says brighten, the highlights say no.
    let mut a = StatsAccumulator::new(4, 3, 64, 1.0);
    for i in 0..100 {
        let v = if i < 95 { 0.02 } else { 0.7 };
        a.add(i % 4, (i / 4) % 3, v, v, v);
    }
    let (gain, target_y, _) =
        agc.compute_gain(&a.finish(), &meta(0, 10.0, 1.0), &Params::default());
    assert!(gain <= 0.8 / 0.7 + 0.05, "{gain}");
    assert_eq!(target_y, 0.8);
}

#[test]
fn fixed_values_and_ae_off() {
    let mut agc = prepared(no_constraints());
    let mut p = Params::default();
    let mut m = meta(0, 10.0, 2.0);
    m.controls.exposure = Some(Duration::from_millis(5));
    m.controls.analogue_gain = Some(3.0);
    agc.process(&grey(0.5), &m, &mut p);
    let r = p.sensor.unwrap();
    assert!((r.exposure.as_secs_f64() - 0.005).abs() < 1e-9 && r.analogue_gain == 3.0);
    // AE off freezes the last values whatever the image does.
    let mut m = meta(20, 5.0, 3.0);
    m.controls.ae_enable = false;
    agc.process(&grey(0.01), &m, &mut p);
    let r = p.sensor.unwrap();
    assert!((r.exposure.as_secs_f64() - 0.005).abs() < 1e-9 && r.analogue_gain == 3.0);
}

#[test]
fn frame_duration_follows_exposure_within_limits() {
    let mut agc = Agc::new(no_constraints()).unwrap();
    let margin = Duration::from_micros(500);
    agc.prepare(&CameraConfig {
        exposure_margin: margin,
        frame_duration_limits: (Duration::from_millis(20), Duration::from_millis(50)),
        ..Default::default()
    })
    .unwrap();
    let mut p = Params::default();
    agc.process(&grey(0.005), &meta(0, 30.0, 1.0), &mut p);
    let r = p.sensor.unwrap();
    assert!(r.exposure <= Duration::from_micros(49_501));
    assert!(r.frame_duration + Duration::from_micros(1) >= r.exposure + margin);
    let mut m = meta(10, 1.0, 1.0);
    m.controls.frame_duration_limits = Some((Duration::from_millis(25), Duration::from_millis(30)));
    agc.hold_until = None;
    agc.process(&grey(0.005), &m, &mut p);
    let r = p.sensor.unwrap();
    assert!(r.frame_duration <= Duration::from_millis(30));
    assert!(r.exposure <= Duration::from_micros(29_501));
}
