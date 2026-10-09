//! Getting to a good image fast: start-up and steps with the device's timing (requests written
//! in the frame their statistics come from), warm starts and mode switches.
//!
//! Run with `--nocapture` to see the measurements.

use crate::common;

use std::time::Duration;

use styx_algo::sim::{Scene, SimFrame, convergence};
use styx_algo::{CameraConfig, ControlDelays, Pipeline, WarmStart};

/// The OV9782 at 30 fps as the PiSP path drives it: exposure and gain land 2 frames after the
/// frame they are written in, frame length 1, and requests are written right after the
/// statistics, in the same frame.
fn device(fps: f64) -> CameraConfig {
    let fd = Duration::from_secs_f64(1.0 / fps);
    CameraConfig {
        exposure_limits: (Duration::from_micros(20), fd - Duration::from_micros(200)),
        exposure_margin: Duration::from_micros(200),
        frame_duration_limits: (fd, fd),
        analogue_gain_limits: (1.0, 15.9375),
        delays: ControlDelays {
            exposure: 2,
            analogue_gain: 2,
            frame_duration: 1,
            issue_latency: 0,
        },
        ..Default::default()
    }
}

fn run(
    config: &CameraConfig,
    scene: Scene,
    warm: Option<&WarmStart>,
    frames: u64,
) -> (Vec<SimFrame>, Pipeline, u64) {
    let tuning = common::tuning();
    let mut p = Pipeline::from_tuning(&tuning).unwrap();
    let init = p.prepare_warm(config, warm).unwrap().clone();
    let mut sim = styx_algo::sim::Simulation::new(Default::default(), scene, config);
    let s = init.sensor.unwrap();
    sim.start_with(
        s.exposure.as_secs_f64(),
        s.analogue_gain,
        s.frame_duration.as_secs_f64(),
    );
    let out = sim.run(&mut p, frames);
    let late = sim.late_landings();
    (out, p, late)
}

fn first_lock(frames: &[SimFrame], from: usize) -> Option<usize> {
    frames[from..].iter().position(|f| f.params.ae.locked)
}

#[test]
fn start_up_with_same_frame_writes() {
    for lux in [20.0, 400.0] {
        let (frames, _, late) = run(&device(30.0), Scene::constant(lux, 5000.0), None, 40);
        let c = convergence(&common::luma(&frames), 0, 0.05, 20);
        let lock = first_lock(&frames, 0);
        println!("start-up at {lux} lux, same-frame writes: {c:?}, locked at {lock:?}");
        assert!(c.settle_frames.is_some_and(|f| f <= 4), "{c:?}");
        assert!(lock.is_some_and(|l| l <= 6), "{lock:?}");
        assert!(c.overshoot < 0.03, "{c:?}");
        assert_eq!(late, 0);
    }
}

#[test]
fn steps_with_same_frame_writes() {
    // Darker scenes take one step; brighter ones more while clipped zones hide how bright.
    for (from, to, max) in [(800.0, 200.0, 2), (200.0, 800.0, 6), (20.0, 5000.0, 12)] {
        let mut scene = Scene::constant(from, 5000.0);
        scene.lux = Scene::step(60, from, to);
        let (frames, _, late) = run(&device(30.0), scene, None, 140);
        let c = convergence(&common::luma(&frames), 60, 0.05, 20);
        let lock = first_lock(&frames, 60);
        println!("{from} -> {to} lux, same-frame writes: {c:?}, locked after {lock:?}");
        assert!(c.settle_frames.is_some_and(|f| f <= max), "{c:?}");
        assert!(c.overshoot < 0.03, "{c:?}");
        assert_eq!(late, 0);
    }
}

#[test]
fn a_warm_restart_does_not_converge_again() {
    let scene = Scene::constant(150.0, 3500.0);
    let (_, first, _) = run(&device(30.0), scene.clone(), None, 40);
    let warm = first.warm_state().unwrap();
    assert!(warm.ae_locked);
    let (frames, _, _) = run(&device(30.0), scene, Some(&warm), 20);
    let y = common::luma(&frames);
    let c = convergence(&y, 0, 0.03, 10);
    let lock = first_lock(&frames, 0);
    println!("warm restart: {c:?}, locked at {lock:?}");
    assert_eq!(c.settle_frames, Some(0), "{c:?}");
    assert!(lock.is_some_and(|l| l <= 1), "{lock:?}");
    // Nothing new is asked for, and the white balance starts where it was.
    assert!(
        frames
            .iter()
            .all(|f| f.params.sensor == frames[0].params.sensor)
    );
    let g = frames[0].params.colour_gains;
    assert!((g[0] / warm.colour_gains[0] - 1.0).abs() < 0.02, "{g:?}");
}

#[test]
fn a_mode_switch_keeps_the_exposure() {
    // 30 fps settled at a long exposure; 120 fps cannot expose that long, so the same total
    // comes from gain.
    let scene = Scene::constant(60.0, 4000.0);
    let (frames30, first, _) = run(&device(30.0), scene.clone(), None, 40);
    let warm = first.warm_state().unwrap();
    let (frames, _, _) = run(&device(120.0), scene, Some(&warm), 30);
    let before = convergence(&common::luma(&frames30), 0, 0.05, 10).final_value;
    let y0 = frames[0].raw_y;
    let lock = first_lock(&frames, 0);
    println!(
        "30 -> 120 fps: {:.1} ms x {:.2} -> {:.1} ms x {:.2}, luma {before:.4} -> {y0:.4}, locked at {lock:?}",
        warm.exposure.as_secs_f64() * 1e3,
        warm.analogue_gain,
        frames[0].meta.exposure.as_secs_f64() * 1e3,
        frames[0].meta.analogue_gain
    );
    assert!(frames[0].meta.exposure < Duration::from_millis(9));
    assert!((y0 / before - 1.0).abs() < 0.05, "{y0} {before}");
    assert!(lock.is_some_and(|l| l <= 2), "{lock:?}");
}

#[test]
fn a_black_level_error_costs_one_correction() {
    // Luma = k × exposure + 0.01: a proportional step misses, the first frame it produces
    // shows by how much, and that is corrected at once (not damped).
    let mut scene = Scene::constant(200.0, 5000.0);
    scene.lux = Scene::step(60, 200.0, 50.0);
    let config = device(30.0);
    let mut p = Pipeline::from_tuning(&common::tuning()).unwrap();
    let init = p.prepare(&config).unwrap().clone();
    let model = styx_algo::sim::SensorModel {
        black_error: 0.01,
        ..Default::default()
    };
    let mut sim = styx_algo::sim::Simulation::new(model, scene, &config);
    let s = init.sensor.unwrap();
    sim.start_with(
        s.exposure.as_secs_f64(),
        s.analogue_gain,
        s.frame_duration.as_secs_f64(),
    );
    let frames = sim.run(&mut p, 120);
    let c = convergence(&common::luma(&frames), 60, 0.03, 20);
    let lock = first_lock(&frames, 60);
    println!("200 -> 50 lux with a black level error: {c:?}, locked after {lock:?}");
    assert!(c.settle_frames.is_some_and(|f| f <= 4), "{c:?}");
    assert!(lock.is_some_and(|l| l <= 6), "{lock:?}");
}

#[test]
fn ae_locks_at_its_limits_in_the_dark() {
    // Too dark for the longest exposure at the highest gain: AE settles at its limits and
    // reports it (locked, at the limit) instead of searching for ever; light again, it leaves.
    let mut scene = Scene::constant(0.05, 3000.0);
    scene.lux = Scene::step(60, 0.05, 200.0);
    let (frames, _, late) = run(&device(30.0), scene, None, 100);
    let lock = first_lock(&frames, 0);
    println!("dark start: locked at {lock:?}, {:?}", frames[59].params.ae);
    assert!(lock.is_some_and(|l| l <= 8), "{lock:?}");
    assert!(
        frames[20..60]
            .iter()
            .all(|f| f.params.ae.locked && f.params.ae.at_limit)
    );
    let relock = first_lock(&frames, 61);
    println!(
        "then 200 lux: locked after {relock:?}, {:?}",
        frames[99].params.ae
    );
    assert!(!frames[99].params.ae.at_limit && frames[99].params.ae.locked);
    assert_eq!(late, 0);
}
