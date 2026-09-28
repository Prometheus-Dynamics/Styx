//! Shared set-up for the simulation tests.
#![allow(dead_code)]

use std::time::Duration;

use styx_algo::sim::{Scene, SensorModel, SimFrame, Simulation};
use styx_algo::{CameraConfig, ControlDelays, Pipeline, Tuning};

/// A 30-10 fps mode with the default delays (exposure 2, gain 1, frame length 2).
pub fn config() -> CameraConfig {
    CameraConfig {
        exposure_limits: (Duration::from_micros(20), Duration::from_millis(100)),
        exposure_margin: Duration::from_micros(200),
        frame_duration_limits: (Duration::from_nanos(33_333_333), Duration::from_millis(100)),
        analogue_gain_limits: (1.0, 16.0),
        delays: ControlDelays::default(),
        ..Default::default()
    }
}

/// The simulation test tuning (Raspberry Pi imx219 AWB data, see the file).
pub fn tuning() -> Tuning {
    Tuning::from_toml_str(include_str!("../data/sim.toml")).unwrap()
}

/// Prepare a pipeline and a simulation starting from its start-up values.
pub fn start(tuning: &Tuning, config: &CameraConfig, scene: Scene) -> (Pipeline, Simulation) {
    let mut p = Pipeline::from_tuning(tuning).unwrap();
    let init = p.prepare(config).unwrap().clone();
    let mut sim = Simulation::new(SensorModel::default(), scene, config);
    if let Some(s) = init.sensor {
        sim.start_with(
            s.exposure.as_secs_f64(),
            s.analogue_gain,
            s.frame_duration.as_secs_f64(),
        );
    }
    (p, sim)
}

/// Raw luma per frame.
pub fn luma(frames: &[SimFrame]) -> Vec<f64> {
    frames.iter().map(|f| f.raw_y).collect()
}
