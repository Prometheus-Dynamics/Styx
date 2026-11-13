//! The shared test double: a `camera.sensor/1` sensor that keeps the mode's delays, and the mode and
//! arithmetic the proofs use. Shared by `landing.rs` and `limits.rs`.
//!
//! The sensor is a test double of `camera.sensor/1` (no daemon, no driver): a `post_values`
//! written during frame `w` shows from frame `w + delay` for each control, the way the mode's
//! `exposure_delay`, `gain_delay` and `frame_length_delay` say. Each frame is rendered as one
//! brightness value, exposure code times gain code, so the image shows which values were in force.
//!
//! `styx-sim` is the simulated sensor for the backend proof in `sim_backend.rs`; this double keeps
//! the arithmetic of each test independent of the daemon. Its gain law is `Linear`, set by the test
//! (the contract has no gain law at the lock).
#![allow(dead_code)] // each test binary uses part of this.

use lemnos_contract_camera_sensor::{
    BayerOrder, CameraSensor, Controls, FrameValues, GainRange, Mode, SensorInfo,
};
use lemnos_core::{Result, Status};
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

/// A `camera.sensor/1` double that applies each posted control at the frame it was written in
/// plus that control's delay. `frame` is the frame the sensor is in; the test sets it.
#[derive(Debug)]
pub struct Sensor {
    delays: [u64; 3],
    pub frame: u64,
    /// `(first frame shown, code)` per control, in landing order; index 0 is the initial value.
    pub exposure: Vec<(u64, u32)>,
    pub gain: Vec<(u64, u32)>,
    pub frame_length: Vec<(u64, u32)>,
    /// Frame each `post_values` was made in, with the identity Styx passed (opaque to Lemnos).
    pub posts: Vec<(u64, u64)>,
}

impl Sensor {
    pub fn new(mode: &Mode) -> Self {
        Self {
            delays: [
                u64::from(mode.exposure_delay),
                u64::from(mode.gain_delay),
                u64::from(mode.frame_length_delay),
            ],
            frame: 0,
            exposure: Vec::new(),
            gain: Vec::new(),
            frame_length: Vec::new(),
            posts: Vec::new(),
        }
    }

    fn in_force(list: &[(u64, u32)], frame: u64) -> u32 {
        list.iter()
            .rev()
            .find(|&&(first, _)| first <= frame)
            .or(list.first())
            .map_or(0, |&(_, code)| code)
    }

    pub fn exposure_at(&self, frame: u64) -> u32 {
        Self::in_force(&self.exposure, frame)
    }

    pub fn gain_at(&self, frame: u64) -> u32 {
        Self::in_force(&self.gain, frame)
    }

    /// The frame's brightness: the exposure code times the gain code in force on it.
    pub fn brightness(&self, frame: u64) -> u64 {
        u64::from(self.exposure_at(frame)) * u64::from(self.gain_at(frame))
    }
}

impl CameraSensor for Sensor {
    fn identity(&mut self, _into: &mut [u8]) -> Result<usize> {
        Err(Status::NOT_SUPPORTED)
    }
    fn info(&mut self) -> Result<SensorInfo> {
        Ok(SensorInfo::default())
    }

    fn mode(&mut self, _index: u16) -> Result<Mode> {
        Err(Status::NOT_FOUND)
    }

    fn select_mode(&mut self, _index: u16) -> Result<()> {
        Ok(())
    }

    fn selected(&mut self) -> Result<Option<u16>> {
        Ok(Some(0))
    }

    fn start(&mut self) -> Result<()> {
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        Ok(())
    }

    /// Before streaming: the value is shown from frame 0.
    fn set_controls(&mut self, controls: Controls) -> Result<()> {
        if let Some(e) = controls.exposure {
            self.exposure = vec![(0, e)];
        }
        if let Some(g) = controls.analogue_gain {
            self.gain = vec![(0, g)];
        }
        if let Some(fl) = controls.frame_length {
            self.frame_length = vec![(0, fl)];
        }
        Ok(())
    }

    fn post_values(&mut self, values: FrameValues) -> Result<()> {
        let c = values.controls;
        if let Some(e) = c.exposure {
            self.exposure.push((self.frame + self.delays[0], e));
        }
        if let Some(g) = c.analogue_gain {
            self.gain.push((self.frame + self.delays[1], g));
        }
        if let Some(fl) = c.frame_length {
            self.frame_length.push((self.frame + self.delays[2], fl));
        }
        self.posts.push((self.frame, values.identity));
        Ok(())
    }

    fn applied(&mut self) -> Result<Option<FrameValues>> {
        Ok(None)
    }

    fn set_flips(&mut self, _horizontal: bool, _vertical: bool) -> Result<BayerOrder> {
        Err(Status::NOT_SUPPORTED)
    }

    fn set_test_pattern(&mut self, _code: u16) -> Result<()> {
        Err(Status::NOT_SUPPORTED)
    }
}

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
