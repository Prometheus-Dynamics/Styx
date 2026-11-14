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
    BayerOrder, CLAMP_ANALOGUE_GAIN, CLAMP_EXPOSURE, CLAMP_FRAME_LENGTH, CameraSensor, Controls,
    FrameValues, GainRange, InForce, Mode, SensorInfo,
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
        readout_ps: 0,
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

/// Microseconds of an exposure code as the driver reports it: in whole lines (the contract's
/// `InForce::exposure_lines`), so the sub-line part of the code is not in the report.
pub fn report_us(code: u64) -> u32 {
    us_for_code((code >> FRACTION) << FRACTION) as u32
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
    /// The mode's limits: what the double clamps to, as the driver does.
    mode: Mode,
    /// The `CLAMP_*` bits of the last request.
    clamped: u8,
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
            mode: mode.clone(),
            clamped: 0,
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

    /// The request as the double (a driver) takes it: each value moved to the nearest one the mode
    /// allows, and the `CLAMP_*` bits of what was moved. The exposure ceiling is the frame less the
    /// mode's margin, in the frame length asked for or last written; a shorter frame also clamps the
    /// exposure in force.
    fn fit(&mut self, controls: Controls) -> Controls {
        let m = self.mode.clone();
        let mut clamped = 0;
        let frame_length = controls.frame_length.map(|lines| {
            let fit = lines.clamp(m.frame_length_min, m.frame_length_max);
            if fit != lines {
                clamped |= CLAMP_FRAME_LENGTH;
            }
            fit
        });
        let fl_now = frame_length
            .or_else(|| self.frame_length.last().map(|&(_, fl)| fl))
            .unwrap_or(m.frame_length_min);
        let exposure_max = (fl_now.saturating_sub(m.exposure_margin) << FRACTION).max(m.exposure_min);
        let asked = match controls.exposure {
            Some(code) => Some(code),
            None if frame_length.is_some() => self.exposure.last().map(|&(_, e)| e),
            None => None,
        };
        let exposure = asked.map(|code| {
            let fit = code.clamp(m.exposure_min, exposure_max);
            if fit != code {
                clamped |= CLAMP_EXPOSURE;
            }
            fit
        });
        let analogue_gain = controls.analogue_gain.map(|code| {
            let fit = code.clamp(m.analogue_gain.min, m.analogue_gain.max);
            if fit != code {
                clamped |= CLAMP_ANALOGUE_GAIN;
            }
            fit
        });
        self.clamped = clamped;
        Controls {
            exposure,
            analogue_gain,
            frame_length,
            digital_gain: controls.digital_gain,
        }
    }

    /// The frame's brightness: the exposure code times the gain code in force on it.
    pub fn brightness(&self, frame: u64) -> u64 {
        u64::from(self.exposure_at(frame)) * u64::from(self.gain_at(frame))
    }
}

impl CameraSensor for Sensor {
    // Added at Lemnos 58098f97: this simulated driver has no landings, so it answers NOT_SUPPORTED.
    fn post_values_at(
        &mut self,
        _values: lemnos_contract_camera_sensor::FrameValues,
    ) -> core::result::Result<lemnos_contract_camera_sensor::Landing, lemnos_core::Status> {
        Err(lemnos_core::Status::NOT_SUPPORTED)
    }

    fn applied_landing(
        &mut self,
    ) -> core::result::Result<
        Option<lemnos_contract_camera_sensor::AppliedLanding>,
        lemnos_core::Status,
    > {
        Err(lemnos_core::Status::NOT_SUPPORTED)
    }

    fn in_force(&mut self) -> core::result::Result<InForce, lemnos_core::Status> {
        let frame = self.frame;
        Ok(InForce {
            exposure_lines: (!self.exposure.is_empty())
                .then(|| self.exposure_at(frame) >> FRACTION),
            analogue_gain: (!self.gain.is_empty()).then(|| self.gain_at(frame)),
            frame_length: (!self.frame_length.is_empty())
                .then(|| Self::in_force(&self.frame_length, frame)),
            clamped: self.clamped,
        })
    }

    fn set_exposure(
        &mut self,
        _lines: u32,
    ) -> core::result::Result<lemnos_contract_camera_sensor::Landing, lemnos_core::Status> {
        Err(lemnos_core::Status::NOT_SUPPORTED)
    }

    fn set_analogue_gain(
        &mut self,
        _code: u32,
    ) -> core::result::Result<lemnos_contract_camera_sensor::Landing, lemnos_core::Status> {
        Err(lemnos_core::Status::NOT_SUPPORTED)
    }

    fn set_frame_length(
        &mut self,
        _lines: u32,
    ) -> core::result::Result<lemnos_contract_camera_sensor::Landing, lemnos_core::Status> {
        Err(lemnos_core::Status::NOT_SUPPORTED)
    }

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
        let c = self.fit(controls);
        if let Some(e) = c.exposure {
            self.exposure = vec![(0, e)];
        }
        if let Some(g) = c.analogue_gain {
            self.gain = vec![(0, g)];
        }
        if let Some(fl) = c.frame_length {
            self.frame_length = vec![(0, fl)];
        }
        Ok(())
    }

    fn post_values(&mut self, values: FrameValues) -> Result<()> {
        let c = self.fit(values.controls);
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
