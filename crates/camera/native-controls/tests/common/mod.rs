//! The shared test helpers: the mode and the arithmetic the proofs use, and the frame-driven sensor
//! double (`styx_sim::ManualSensor`, no daemon). Shared by `landing.rs`, `limits.rs`, `clamp.rs` and
//! `gain_law.rs`. The gain law is `Linear`, set by the test (the contract has no gain law at the lock).
#![allow(dead_code)] // each test binary uses part of this.

use lemnos_contract_camera_sensor::{GainRange, Mode};
use styx_controls::{Control, ControlValue, Update};
use styx_native_controls::{Error, Initial, NativeControls};

/// Picoseconds per line: an a 728-clock line at 80 MHz.
pub const LINE_TIME_PS: u32 = 9_100_000;
/// Fractional bits of the exposure code.
pub const FRACTION: u8 = 4;

pub fn mode(exposure_delay: u8, gain_delay: u8, frame_length_delay: u8) -> Mode {
    Mode {
        line_time_ps: LINE_TIME_PS,
        line_length: 1456,
        frame_length_min: 900,
        frame_length_max: 52_000,
        exposure_min: 16,
        exposure_margin: 25,
        exposure_fraction_bits: FRACTION,
        analogue_gain: GainRange {
            min: 16,
            max: 255,
            unity: 16,
            step: 1,
            law: lemnos_contract_camera_sensor::GainLaw::Linear,
        },
        exposure_delay,
        gain_delay,
        frame_length_delay,
        ..Mode::default()
    }
}

/// Exposure code for `us`, written out here (not shared with the crate) so the test checks the
/// crate's arithmetic against its own.
pub fn code_for_us(us: u64) -> u64 {
    (us * 1_000_000 * (1 << FRACTION) + u64::from(LINE_TIME_PS) / 2) / u64::from(LINE_TIME_PS)
}

/// Microseconds for an exposure code, written out here the same way.
pub fn us_for_code(code: u64) -> u64 {
    (code * u64::from(LINE_TIME_PS) + 8_000_000) / 16_000_000
}

/// Gain code for thousandths of gain at unity 16.
pub fn code_for_milli(milli: u64) -> u64 {
    (milli * 16 + 500) / 1000
}

/// The sensor double (`styx_sim::ManualSensor`), driven frame by frame by each test.
pub use styx_sim::ManualSensor as Sensor;

pub const START: Initial = Initial {
    exposure_us: 8_000,
    gain_milli: 1_000,
    frame_rate_milli: 60_000,
};

pub fn update(control: Control, value: u32, frame: u64) -> Update {
    Update {
        control,
        value: ControlValue::Manual(value),
        frame,
    }
}

/// Runs frames `0..=end`, making `updates` at frame `request_at`, and returns what happened.
#[derive(Debug)]
pub struct Run {
    pub sensor: Sensor,
    pub controls: NativeControls,
    pub landings: Vec<(Control, u64, u64)>,
}

pub fn run(
    mode: &Mode,
    request_at: u64,
    updates: &[Update],
    end: u64,
) -> std::result::Result<Run, Error> {
    let mut controls = NativeControls::new(mode, START)?;
    let mut sensor = Sensor::new(mode);
    controls.prepare(&mut sensor)?;
    let mut landings = Vec::new();
    for k in 0..=end {
        sensor.frame = k;
        controls.frame_start(&mut sensor, k)?;
        if k == request_at {
            let got = controls.request(&mut sensor, updates)?;
            landings = got
                .iter()
                .map(|l| (l.control, l.requested, l.frame))
                .collect();
        }
    }
    Ok(Run {
        sensor,
        controls,
        landings,
    })
}
