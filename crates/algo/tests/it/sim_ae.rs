//! AE convergence in the simulator: settle time, overshoot, stability, flicker, frame timing.
//!
//! Run with `--nocapture` to see the measurements.

use crate::common;

use std::time::Duration;

use styx_algo::sim::{Scene, SceneFlicker, SimFrame, convergence};
use styx_algo::{Flicker, metering};

const STEP: u64 = 80;

fn step_run(from: f64, to: f64, frames: u64) -> (Vec<SimFrame>, u64) {
    let mut scene = Scene::constant(from, 5000.0);
    scene.lux = Scene::step(STEP, from, to);
    let (mut p, mut sim) = common::start(&common::tuning(), &common::config(), scene);
    let out = sim.run(&mut p, frames);
    (out, sim.late_landings())
}

#[test]
fn start_up_settles_quickly() {
    let (frames, late) = step_run(20.0, 20.0, 60);
    let c = convergence(&common::luma(&frames), 0, 0.05, 20);
    println!("start-up at 20 lux: {c:?}");
    assert!(c.settle_frames.is_some_and(|f| f <= 15), "{c:?}");
    assert!(c.jitter < 0.01);
    assert_eq!(late, 0);
}

#[test]
fn dark_to_bright_settles_without_overshoot() {
    let (frames, late) = step_run(20.0, 5000.0, 200);
    let y = common::luma(&frames);
    let before = convergence(&y[..STEP as usize], 0, 0.05, 20);
    let c = convergence(&y, STEP as usize, 0.05, 20);
    println!("20 -> 5000 lux: {c:?}");
    assert!(c.settle_frames.is_some_and(|f| f <= 35), "{c:?}");
    // Coming down from saturation: no dip below the final level.
    assert!(c.overshoot < 0.05, "{c:?}");
    assert!(c.jitter < 0.01, "{c:?}");
    // The same image brightness in both scenes.
    assert!((c.final_value / before.final_value - 1.0).abs() < 0.05);
    assert!(frames.last().unwrap().params.ae.locked);
    assert_eq!(late, 0);
}

#[test]
fn bright_to_dark_settles_without_overshoot() {
    let (frames, late) = step_run(5000.0, 20.0, 220);
    let y = common::luma(&frames);
    let c = convergence(&y, STEP as usize, 0.05, 40);
    println!("5000 -> 20 lux: {c:?}");
    assert!(c.settle_frames.is_some_and(|f| f <= 60), "{c:?}");
    assert!(c.overshoot < 0.05, "{c:?}");
    assert!(c.jitter < 0.01, "{c:?}");
    assert_eq!(late, 0);
}

#[test]
fn small_steps_are_damped_and_stable() {
    let (frames, _) = step_run(200.0, 214.0, 200);
    let y = common::luma(&frames);
    let c = convergence(&y, STEP as usize, 0.03, 40);
    println!("200 -> 214 lux: {c:?}");
    assert!(c.settle_frames.is_some_and(|f| f <= 30), "{c:?}");
    assert!(c.overshoot < 0.03, "{c:?}");
    // Damped: the first change is partial.
    let total = |f: &SimFrame| f.meta.exposure.as_secs_f64() * f.meta.analogue_gain;
    let first = total(&frames[STEP as usize + 4]) / total(&frames[STEP as usize]);
    assert!(first > 0.93 && first < 0.985, "{first}");
}

#[test]
fn large_steps_go_straight_to_the_target() {
    let (frames, late) = step_run(200.0, 300.0, 200);
    let y = common::luma(&frames);
    let c = convergence(&y, STEP as usize, 0.03, 40);
    println!("200 -> 300 lux: {c:?}");
    // One step: the request from the first brighter frame lands 4 frames later.
    assert!(c.settle_frames.is_some_and(|f| f <= 5), "{c:?}");
    assert!(c.overshoot < 0.03, "{c:?}");
    assert_eq!(late, 0);
}

/// Every request lands whole (exposure and gain together) on the frame it names.
#[test]
fn requests_land_on_the_frame_they_name() {
    let (frames, late) = step_run(20.0, 2000.0, 160);
    assert_eq!(late, 0);
    let config = common::config();
    let line = 20e-6;
    let mut checked = 0;
    for (i, f) in frames.iter().enumerate() {
        let r = f.params.sensor.unwrap();
        let repeat = i > 0 && frames[i - 1].params.sensor == f.params.sensor;
        if !repeat {
            assert_eq!(r.frame, config.delays.earliest_landing(f.meta.frame));
        }
        let Some(land) = frames.get(r.frame as usize).filter(|_| !repeat) else {
            continue;
        };
        assert_eq!(
            land.meta.analogue_gain, r.analogue_gain,
            "frame {}",
            r.frame
        );
        let want = (r.exposure.as_secs_f64() / line).floor() * line;
        let got = land.meta.exposure.as_secs_f64();
        assert!(
            (got - want).abs() < line * 0.01,
            "frame {}: {got} vs {want}",
            r.frame
        );
        checked += 1;
    }
    // Only changes are new requests (repeats keep their frame).
    assert!(checked > 4, "{checked}");
}

fn flicker_run(flicker: Flicker) -> Vec<SimFrame> {
    let mut scene = Scene::constant(100.0, 4000.0);
    scene.flicker = Some(SceneFlicker {
        hz: 100.0,
        depth: 0.5,
    });
    let (mut p, mut sim) = common::start(&common::tuning(), &common::config(), scene);
    sim.controls.flicker = flicker;
    sim.run(&mut p, 150)
}

#[test]
fn flicker_avoidance_uses_whole_periods() {
    let off = flicker_run(Flicker::Off);
    let on = flicker_run(Flicker::Mains50);
    let c_off = convergence(&common::luma(&off), 0, 0.05, 60);
    let c_on = convergence(&common::luma(&on), 0, 0.05, 60);
    println!("flicker 100 Hz, avoidance off: {c_off:?}");
    println!("flicker 100 Hz, avoidance 50 Hz: {c_on:?}");
    assert!(c_off.jitter > 0.01, "{c_off:?}");
    assert!(c_on.jitter < 0.005, "{c_on:?}");
    for f in &on[60..] {
        let ms = f.meta.exposure.as_secs_f64() * 1e3;
        assert!(
            ms >= 10.0 - 0.03 && (ms / 10.0 - (ms / 10.0).round()).abs() < 0.003,
            "{ms}"
        );
    }
}

#[test]
fn ev_and_metering_modes_change_exposure() {
    // Without histogram constraints, so metering alone decides.
    let mut tuning = common::tuning();
    let mut agc = tuning.agc.clone().unwrap_or_default();
    agc.constraint_modes.insert("normal".into(), Vec::new());
    tuning.agc = Some(agc);
    let run = |ev: f64, mode: &str| {
        let scene = Scene::constant(100.0, 5000.0);
        let (mut p, mut sim) = common::start(&tuning, &common::config(), scene);
        sim.controls.ev = ev;
        sim.controls.metering_mode = Some(mode.into());
        let f = sim.run(&mut p, 80);
        convergence(&common::luma(&f), 0, 0.05, 20).final_value
    };
    let base = run(0.0, metering::CENTRE_WEIGHTED);
    let plus = run(1.0, metering::CENTRE_WEIGHTED);
    println!("EV 0: {base:.4}, EV +1: {plus:.4}");
    assert!((plus / base - 2.0).abs() < 0.3, "{base} {plus}");
    // The default surfaces are brighter towards the bottom right, so metering on the centre
    // and on the whole frame give different exposures.
    let spot = run(0.0, metering::SPOT);
    let average = run(0.0, metering::AVERAGE);
    println!("spot {spot:.4} average {average:.4}");
    assert!((spot / average - 1.0).abs() > 0.02);
}

#[test]
fn fixed_exposure_moves_only_gain() {
    let scene = Scene::constant(100.0, 5000.0);
    let (mut p, mut sim) = common::start(&common::tuning(), &common::config(), scene);
    sim.controls.exposure = Some(Duration::from_millis(5));
    let frames = sim.run(&mut p, 60);
    let last = frames.last().unwrap();
    assert!((last.meta.exposure.as_secs_f64() - 0.005).abs() < 21e-6);
    assert!(last.meta.analogue_gain > 1.5);
    let c = convergence(&common::luma(&frames), 0, 0.05, 20);
    assert!(c.settle_frames.is_some_and(|f| f <= 20), "{c:?}");
}

#[test]
fn exposure_mode_limits_exposure_time() {
    let scene = Scene::constant(5.0, 5000.0);
    let (mut p, mut sim) = common::start(&common::tuning(), &common::config(), scene);
    sim.controls.exposure_mode = Some("short".into());
    let frames = sim.run(&mut p, 80);
    let last = frames.last().unwrap();
    assert!(last.meta.exposure <= Duration::from_micros(33_334));
    assert!(last.meta.analogue_gain > 4.0);
    // The frame duration follows the exposure within the limits.
    assert!(last.meta.frame_duration >= last.meta.exposure);
}

#[test]
fn simulation_is_deterministic() {
    let a = step_run(20.0, 5000.0, 120).0;
    let b = step_run(20.0, 5000.0, 120).0;
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.params, y.params);
        assert_eq!(x.stats, y.stats);
    }
}
