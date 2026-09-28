//! AWB convergence in the simulator. Run with `--nocapture` to see the measurements.

mod common;

use styx_algo::sim::{Scene, SensorModel, SimFrame, convergence};
use styx_algo::{Controls, Tuning};

const STEP: u64 = 60;

/// Gains that make a grey surface grey under `ct` for the simulated sensor.
fn truth(ct: f64) -> (f64, f64) {
    let (r, _, b) = SensorModel::default().illuminant(ct);
    (1.0 / r, 1.0 / b)
}

fn run(tuning: &Tuning, lux: f64, from: f64, to: f64, frames: u64) -> Vec<SimFrame> {
    let mut scene = Scene::constant(lux, from);
    scene.ct = Scene::step(STEP, from, to);
    let (mut p, mut sim) = common::start(tuning, &common::config(), scene);
    sim.run(&mut p, frames)
}

fn series(frames: &[SimFrame], f: impl Fn(&SimFrame) -> f64) -> Vec<f64> {
    frames.iter().map(f).collect()
}

fn check_step(tuning: &Tuning, lux: f64, from: f64, to: f64) {
    let frames = run(tuning, lux, from, to, 260);
    let r = convergence(
        &series(&frames, |f| f.params.colour_gains[0]),
        STEP as usize,
        0.03,
        30,
    );
    let b = convergence(
        &series(&frames, |f| f.params.colour_gains[2]),
        STEP as usize,
        0.03,
        30,
    );
    let ct = frames.last().unwrap().params.colour_temperature;
    let (tr, tb) = truth(to);
    println!(
        "{from} K -> {to} K at {lux} lux: ct {ct:.0}, red {r:?} (truth {tr:.3}), blue {b:?} (truth {tb:.3})"
    );
    assert!(
        (r.final_value / tr - 1.0).abs() < 0.05,
        "red {} vs {tr}",
        r.final_value
    );
    assert!(
        (b.final_value / tb - 1.0).abs() < 0.05,
        "blue {} vs {tb}",
        b.final_value
    );
    assert!((ct / to - 1.0).abs() < 0.12, "ct {ct} vs {to}");
    for c in [r, b] {
        assert!(c.settle_frames.is_some_and(|f| f <= 120), "{c:?}");
        assert!(c.overshoot < 0.03, "{c:?}");
        assert!(c.jitter < 0.01, "{c:?}");
    }
    assert!(frames.last().unwrap().params.awb.converged);
}

#[test]
fn bayes_follows_warm_to_daylight() {
    check_step(&common::tuning(), 400.0, 3000.0, 6000.0);
}

#[test]
fn bayes_follows_daylight_to_warm() {
    check_step(&common::tuning(), 400.0, 6000.0, 3000.0);
}

#[test]
fn start_up_is_immediate() {
    let frames = run(&common::tuning(), 400.0, 4500.0, 4500.0, 20);
    let (tr, tb) = truth(4500.0);
    let g = frames[12].params.colour_gains;
    assert!(
        (g[0] / tr - 1.0).abs() < 0.05 && (g[2] / tb - 1.0).abs() < 0.05,
        "{g:?}"
    );
}

#[test]
fn grey_world_without_a_ct_curve() {
    let t = Tuning::default();
    let frames = run(&t, 400.0, 3000.0, 6000.0, 260);
    let last = &frames.last().unwrap().params;
    let (tr, tb) = truth(6000.0);
    println!(
        "grey world at 6000 K: {:?} (truth {tr:.3}, {tb:.3})",
        last.colour_gains
    );
    assert!((last.colour_gains[0] / tr - 1.0).abs() < 0.08);
    assert!((last.colour_gains[2] / tb - 1.0).abs() < 0.08);
}

#[test]
fn modes_restrict_the_search() {
    // Daylight mode under a 3000 K light still reports a daylight temperature.
    let mut scene = Scene::constant(400.0, 3000.0);
    scene.ct = Scene::step(0, 3000.0, 3000.0);
    let (mut p, mut sim) = common::start(&common::tuning(), &common::config(), scene);
    sim.controls = Controls {
        awb_mode: Some("daylight".into()),
        ..Default::default()
    };
    let frames = sim.run(&mut p, 40);
    let ct = frames.last().unwrap().params.colour_temperature;
    assert!((5400.0..=6600.0).contains(&ct), "{ct}");
}

#[test]
fn manual_gains_and_temperature() {
    let scene = Scene::constant(400.0, 4000.0);
    let (mut p, mut sim) = common::start(&common::tuning(), &common::config(), scene);
    sim.controls = Controls {
        awb_enable: false,
        colour_gains: Some((1.5, 2.0)),
        ..Default::default()
    };
    let f = sim.run(&mut p, 3);
    let last = &f.last().unwrap().params;
    assert_eq!(last.colour_gains, [1.5, 1.0, 2.0]);
    assert!(!last.awb.auto);
    sim.controls = Controls {
        awb_enable: false,
        colour_temperature: Some(5000.0),
        ..Default::default()
    };
    let f = sim.run(&mut p, 1);
    let (tr, tb) = truth(5000.0);
    let g = f[0].params.colour_gains;
    assert!((g[0] - tr).abs() < 1e-9 && (g[2] - tb).abs() < 1e-9);
    // CCM follows the temperature.
    assert_ne!(f[0].params.ccm, styx_algo::IDENTITY);
}
