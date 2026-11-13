//! Shared set-up for the replay tests.
#![allow(dead_code)]

use std::time::Duration;

use styx_aaa::{CameraConfig, ControlDelays, Tuning};

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

/// The replay tests' tuning: the parameters of `data/sim-tuning.json` (a serialised [`Tuning`]).
pub fn tuning() -> Tuning {
    serde_json::from_str(include_str!("../../data/sim-tuning.json")).unwrap()
}
