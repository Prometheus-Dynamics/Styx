//! The native controls of one camera: what is posted, when, and what is in force.

use lemnos_contract_camera_sensor::{
    CLAMP_ANALOGUE_GAIN, CLAMP_EXPOSURE, CLAMP_FRAME_LENGTH, CameraSensor, Controls, FrameValues,
    GainRange, InForce, Mode, fps_milli_of, frame_length_for_fps_milli,
};
use std::collections::VecDeque;

use lemnos_core::Status;
use styx_controls::{Control, ControlValue, FrameMetadata, Update};

use crate::clamp::{Clamp, ClampReason, ExposureReport};
use crate::error::{Error, Limit};
use crate::gain;
use crate::schedule::Lane;
use crate::units;

/// Lane indices: exposure, analogue gain, frame length (the frame rate's form on the sensor).
const EXPOSURE: usize = 0;
const GAIN: usize = 1;
const FRAME_LENGTH: usize = 2;
const LANES: usize = 3;

/// Digital gain is not posted here; every frame reports unity.
const DIGITAL_GAIN_MILLI: u32 = 1_000;

/// The values the sensor starts with, in styx-controls units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Initial {
    /// Exposure, in microseconds.
    pub exposure_us: u32,
    /// Analogue gain, in thousandths (1000 is 1.0x).
    pub gain_milli: u32,
    /// Frame rate, in millihertz.
    pub frame_rate_milli: u32,
}

/// A frame rate as the sensor takes it: the frame length in lines, and the rate that length gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRate {
    /// The frame length, in whole lines.
    pub lines: u32,
    /// The rate the frame length gives, in millihertz: the rate asked for to the nearest whole
    /// line. Once the driver has reported, the rate its frame length gives.
    pub achieved_milli: u32,
    /// The driver clamped the frame length to its limits, as its report says (`CLAMP_FRAME_LENGTH`).
    /// `false` until a report is read: the limits are the driver's to apply, not Styx's.
    pub clamped: bool,
}

/// Where a value was asked to land, and where it is predicted to land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Landing {
    /// The control.
    pub control: Control,
    /// The frame the value was asked to apply from.
    pub requested: u64,
    /// The first frame predicted to show it. Equal to `requested` when the request was on time.
    pub frame: u64,
    /// For a frame rate: the rate the frame length asked for gives, in millihertz. The frames run at
    /// what the driver reports in force, which may differ if it clamps. `None` for other controls.
    pub achieved_milli: Option<u32>,
    /// For a frame rate: whether the driver clamped it, as its report says once the value has
    /// landed. Always `false` in the landing itself: the answer to a request does not know it.
    pub clamped: bool,
}

impl Landing {
    /// The request came too late to land on its frame.
    pub fn late(&self) -> bool {
        self.frame > self.requested
    }
}

/// The landings of one request, one per update, in the order given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Landings {
    items: [Option<Landing>; LANES],
    len: usize,
}

impl Landings {
    /// The landings, in the order the updates were given.
    pub fn iter(&self) -> impl Iterator<Item = &Landing> {
        self.items.iter().flatten()
    }

    /// How many landings there are.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when the request was empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The controls of one native camera. See the [crate documentation](crate).
#[derive(Debug)]
pub struct NativeControls {
    limits: Limits,
    lanes: [Lane; LANES],
    frame: Option<u64>,
    /// The frame rate: the request's own at first, then as the driver reports it in force.
    rate: FrameRate,
    /// The exposure asked for last, in microseconds.
    exposure_requested_us: u32,
    /// The exposure the driver reports in force, in microseconds. `None` until a report says, and
    /// after a request until the next one.
    exposure_applied_us: Option<u32>,
    /// The clamp the driver reported for exposure, if it clamped the last request.
    exposure_clamp: Option<Clamp>,
    /// The gain asked for last, in thousandths.
    gain_requested_milli: u32,
    /// The clamp the driver reported for gain, if it clamped the last request.
    gain_clamp: Option<Clamp>,
    /// The reports read at the start of recent frames, by frame, for the frames' metadata.
    reports: VecDeque<(u64, InForce)>,
}

/// Frames whose reports are kept for their metadata.
const REPORTS: usize = 256;

/// The sensor's mode, as the controls use it: the line time, the exposure's fraction bits, the gain
/// codes and law, and the frame lengths the mode names (for the error of a zero rate).
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// The mode's line time, as the contract has it (`u32` picoseconds).
    line_time_ps: u32,
    fraction_bits: u8,
    /// The analogue gain's codes and law (the contract's `GainRange`).
    gain: GainRange,
    fl_min: u32,
    fl_max: u32,
}

fn lane_of(control: Control) -> Option<usize> {
    match control {
        Control::Exposure => Some(EXPOSURE),
        Control::Gain => Some(GAIN),
        Control::FrameRate => Some(FRAME_LENGTH),
        _ => None,
    }
}

impl Limits {
    fn exposure_us(&self, code: u32) -> u32 {
        units::exposure_us(
            u64::from(code),
            u64::from(self.line_time_ps),
            self.fraction_bits,
        )
    }

    /// The exposure in microseconds for a whole number of lines, as a driver reports it.
    fn exposure_us_of_lines(&self, lines: u32) -> u32 {
        let code = u64::from(lines) << self.fraction_bits;
        units::exposure_us(code, u64::from(self.line_time_ps), self.fraction_bits)
    }

    /// The exposure code for `us`. Not bounded here: the driver clamps it to what its frame allows.
    fn exposure_code(&self, us: u32) -> u32 {
        u32::try_from(units::exposure_code(
            us,
            u64::from(self.line_time_ps),
            self.fraction_bits,
        ))
        .unwrap_or(u32::MAX)
    }

    /// Thousandths of gain for a code. Only called with a code this crate made, under the range's
    /// law that [`NativeControls::new`] has already converted, so the conversion cannot be refused.
    fn gain_milli(&self, code: u32) -> u32 {
        gain::code_to_gain(&self.gain, code).unwrap_or(0)
    }

    /// The gain code for `milli`, by the mode's law. Not bounded here: the driver clamps it to its
    /// codes.
    fn gain_code(&self, milli: u32) -> Result<u32, Error> {
        gain::gain_to_code(&self.gain, milli)
    }

    /// The rate a frame of `lines` lines runs at, in millihertz: the contract's `fps_milli_of`.
    /// Zero lines (no period) is rate 0; a rate too large for `u32` saturates.
    fn rate_of(&self, lines: u32) -> u32 {
        // PER-FRAME TERM: the frame length becomes a period here. A per-frame correction from the
        // contract (for one frame's own period, not the mode's line time) plugs in at this point.
        if lines == 0 {
            return 0;
        }
        fps_milli_of(self.line_time_ps, lines).unwrap_or(u32::MAX)
    }

    /// The frame length for a frame rate, in whole lines: the contract's
    /// `frame_length_for_fps_milli` over every whole line (`1..=u32::MAX`), so the nearest rate.
    /// Not bounded to the mode's frame lengths: the driver clamps it and reports it. Only a rate of
    /// zero is refused.
    fn frame_rate(&self, mhz: u32) -> Result<FrameRate, Error> {
        let lines = frame_length_for_fps_milli(self.line_time_ps, 1, u32::MAX, mhz)
            .map(|(lines, _rate)| lines)
            .ok_or_else(|| {
                Error::Limit(Limit {
                    control: Control::FrameRate,
                    requested: mhz,
                    min: self.rate_of(self.fl_max),
                    max: self.rate_of(self.fl_min),
                    frame_lines: None,
                })
            })?;
        Ok(FrameRate {
            lines,
            achieved_milli: self.rate_of(lines),
            clamped: false,
        })
    }
}

/// The reason a clamped value was moved: up if the sensor took less than was asked, down if more.
fn reason_of(applied: u32, requested: u32) -> ClampReason {
    if applied < requested {
        ClampReason::Ceiling
    } else {
        ClampReason::Floor
    }
}

/// The clamp for a report: the one already held when it says the same (so a clamp that persists
/// costs nothing per frame), a new one when it differs, and none when the driver did not clamp it or
/// applied what was asked.
fn clamp_report(
    held: Option<Clamp>,
    control: Control,
    requested: u32,
    applied: u32,
    frame_lines: Option<u32>,
    clamped: bool,
) -> Option<Clamp> {
    if !clamped || applied == requested {
        return None;
    }
    let reason = reason_of(applied, requested);
    match held {
        Some(clamp)
            if clamp.requested == requested
                && clamp.applied == applied
                && clamp.frame_lines == frame_lines
                && clamp.reason == reason =>
        {
            Some(clamp)
        }
        _ => Some(Clamp::new(control, requested, applied, frame_lines, reason)),
    }
}

impl NativeControls {
    /// The controls of a sensor in `mode`, starting at `initial`. Gain converts by the mode's own
    /// gain law (the contract's `GainRange::law`). Values are posted as asked: the sensor clamps
    /// what it cannot take, and its report says what it applied ([`NativeControls::observe`]).
    /// Refuses an initial value the controls cannot convert, and a mode with no line time or no
    /// gain unity code.
    pub fn new(mode: &Mode, initial: Initial) -> Result<Self, Error> {
        if mode.line_time_ps == 0 {
            return Err(Error::Mode("line_time_ps is zero"));
        }
        if mode.analogue_gain.unity == 0 {
            return Err(Error::Mode("analogue gain has no unity code"));
        }
        let limits = Limits {
            line_time_ps: mode.line_time_ps,
            fraction_bits: mode.exposure_fraction_bits,
            gain: mode.analogue_gain,
            fl_min: mode.frame_length_min,
            fl_max: mode.frame_length_max,
        };
        for (control, value) in [
            (Control::Exposure, initial.exposure_us),
            (Control::Gain, initial.gain_milli),
            (Control::FrameRate, initial.frame_rate_milli),
        ] {
            control.validate(ControlValue::Manual(value))?;
        }
        let rate = limits.frame_rate(initial.frame_rate_milli)?;
        let exposure_code = limits.exposure_code(initial.exposure_us);
        let gain_code = limits.gain_code(initial.gain_milli)?;
        Ok(Self {
            lanes: [
                Lane::new(mode.exposure_delay.into(), exposure_code),
                Lane::new(mode.gain_delay.into(), gain_code),
                Lane::new(mode.frame_length_delay.into(), rate.lines),
            ],
            frame: None,
            rate,
            exposure_requested_us: initial.exposure_us,
            exposure_applied_us: None,
            exposure_clamp: None,
            gain_requested_milli: initial.gain_milli,
            gain_clamp: None,
            reports: VecDeque::with_capacity(REPORTS),
            limits,
        })
    }

    /// Exposure as it stands: the value asked for last, the value the driver reports in force (`None`
    /// until it reports), and the clamp it reported for the last request.
    pub fn exposure_report(&self) -> ExposureReport {
        ExposureReport {
            requested_us: self.exposure_requested_us,
            applied_us: self.exposure_applied_us,
            clamp: self.exposure_clamp.clone(),
        }
    }

    /// The clamp the driver reported for `control` (exposure or gain) on the last request, if it
    /// clamped it. `None` for the other controls, and when it did not.
    pub fn clamp(&self, control: Control) -> Option<&Clamp> {
        match control {
            Control::Exposure => self.exposure_clamp.as_ref(),
            Control::Gain => self.gain_clamp.as_ref(),
            _ => None,
        }
    }

    /// The frame rate: the frame length and the rate that length gives. Before the driver reports it
    /// is the rate asked for; afterwards the driver's frame length and whether it clamped it.
    pub fn initial_rate(&self) -> FrameRate {
        self.rate
    }

    /// Writes the initial values with `set_controls`, before the sensor starts. Frame 0 shows them.
    pub fn prepare<S: CameraSensor + ?Sized>(&mut self, sensor: &mut S) -> Result<(), Error> {
        let controls = Controls {
            exposure: Some(self.lanes[EXPOSURE].value_at(0)),
            analogue_gain: Some(self.lanes[GAIN].value_at(0)),
            frame_length: Some(self.lanes[FRAME_LENGTH].value_at(0)),
            digital_gain: None,
        };
        sensor.set_controls(controls).map_err(Error::Sensor)
    }

    /// Takes the driver's report of what is in force: the exposure, gain and frame length it holds,
    /// and which of them it clamped on the last request. Styx takes the applied values and the fact
    /// of a clamp from here; the reason (ceiling or floor) is read off the requested and applied
    /// values. Exposure is whole lines in the report, so its applied value is to the line.
    pub fn observe(&mut self, in_force: &InForce) {
        if let Some(lines) = in_force.frame_length {
            self.rate = FrameRate {
                lines,
                achieved_milli: self.limits.rate_of(lines),
                clamped: in_force.clamped & CLAMP_FRAME_LENGTH != 0,
            };
        }
        match in_force.exposure_lines {
            Some(lines) => {
                let applied = self.limits.exposure_us_of_lines(lines);
                let clamped = in_force.clamped & CLAMP_EXPOSURE != 0;
                self.exposure_applied_us = Some(applied);
                self.exposure_clamp = clamp_report(
                    self.exposure_clamp.take(),
                    Control::Exposure,
                    self.exposure_requested_us,
                    applied,
                    Some(self.rate.lines),
                    clamped,
                );
            }
            None => {
                self.exposure_applied_us = None;
                self.exposure_clamp = None;
            }
        }
        match in_force.analogue_gain {
            Some(code) => {
                let applied = self.limits.gain_milli(code);
                let clamped = in_force.clamped & CLAMP_ANALOGUE_GAIN != 0;
                self.gain_clamp = clamp_report(
                    self.gain_clamp.take(),
                    Control::Gain,
                    self.gain_requested_milli,
                    applied,
                    None,
                    clamped,
                );
            }
            None => self.gain_clamp = None,
        }
    }

    /// Frame `frame` has started (the frame ids of the frame stream). Posts what is due in it: each
    /// value whose frame is `frame + delay`, grouped by frame and delay so one post holds values that
    /// land together. Then reads what the driver holds in force, so a value posted for a delay of
    /// zero is in that report. A driver that answers `NOT_SUPPORTED` to the report says nothing about
    /// what is in force: the applied values are then unknown, and nothing is refused for it.
    pub fn frame_start<S: CameraSensor + ?Sized>(
        &mut self,
        sensor: &mut S,
        frame: u64,
    ) -> Result<(), Error> {
        self.frame = Some(frame);
        self.post_due(sensor, frame)?;
        self.read_report(sensor, frame)
    }

    /// Reads the driver's report of what is in force, for `frame`: it is observed, and kept for the
    /// frame's metadata (replacing an earlier read of the same frame).
    fn read_report<S: CameraSensor + ?Sized>(
        &mut self,
        sensor: &mut S,
        frame: u64,
    ) -> Result<(), Error> {
        match sensor.in_force() {
            Ok(in_force) => {
                self.observe(&in_force);
                match self.reports.back_mut() {
                    Some(last) if last.0 == frame => last.1 = in_force,
                    _ => {
                        if self.reports.len() == REPORTS {
                            self.reports.pop_front();
                        }
                        self.reports.push_back((frame, in_force));
                    }
                }
            }
            Err(Status::NOT_SUPPORTED) => {
                self.exposure_applied_us = None;
                self.exposure_clamp = None;
                self.gain_clamp = None;
            }
            Err(status) => return Err(Error::Sensor(status)),
        }
        Ok(())
    }

    /// Asks for values from their frames on. Each is posted now if its post frame has come (the
    /// current frame or earlier), and otherwise in the frame that is its delay before it. Returns
    /// where each is predicted to land.
    ///
    /// Nothing is fitted here. A value the sensor cannot take exactly, an exposure past what its
    /// frame allows, a gain past its codes, a frame length past the mode's, is posted as asked: the
    /// driver clamps it and reports the clamp and the applied value on its next report
    /// ([`NativeControls::observe`]), and [`NativeControls::exposure_report`] and
    /// [`NativeControls::clamp`] show that report.
    ///
    /// Refused, with nothing posted: a control with no sensor value here, a value outside the
    /// generic range, an automatic value, a duplicate, and a frame rate of zero.
    pub fn request<S: CameraSensor + ?Sized>(
        &mut self,
        sensor: &mut S,
        updates: &[Update],
    ) -> Result<Landings, Error> {
        let at = self.frame.ok_or(Error::NotStreaming)?;
        let mut asked: [Option<(u64, u32)>; LANES] = [None; LANES];
        let mut exposure_us: Option<u32> = None;
        let mut gain_milli: Option<u32> = None;
        let mut landings = Landings::default();
        for update in updates {
            let control = update.control;
            let lane = lane_of(control).ok_or(Error::Unsupported(control))?;
            let mut rate = None;
            if update.value == ControlValue::Auto {
                return Err(Error::Auto(control));
            }
            control.validate(update.value)?;
            if landings.iter().any(|l| l.control == control) {
                return Err(Error::Duplicate(control));
            }
            let ControlValue::Manual(value) = update.value else {
                return Err(Error::Auto(control));
            };
            let frame = update.frame;
            match lane {
                EXPOSURE => {
                    exposure_us = Some(value);
                    asked[EXPOSURE] = Some((frame, self.limits.exposure_code(value)));
                }
                GAIN => {
                    gain_milli = Some(value);
                    asked[GAIN] = Some((frame, self.limits.gain_code(value)?));
                }
                _ => {
                    let frame_rate = self.limits.frame_rate(value)?;
                    asked[FRAME_LENGTH] = Some((frame, frame_rate.lines));
                    rate = Some(frame_rate);
                }
            }
            // Posted now when its post frame has come; otherwise in the frame `delay` before it.
            let delay = u64::from(self.lanes[lane].delay);
            let landing = if frame.saturating_sub(delay) <= at {
                at + delay
            } else {
                frame
            };
            landings.items[landings.len] = Some(Landing {
                control,
                requested: frame,
                frame: landing,
                achieved_milli: rate.map(|r| r.achieved_milli),
                clamped: false,
            });
            landings.len += 1;
        }

        // A request changes what is asked, so what the driver reported for it is not known until
        // its next report.
        if let Some(us) = exposure_us {
            self.exposure_requested_us = us;
        }
        if exposure_us.is_some() || asked[FRAME_LENGTH].is_some() {
            self.exposure_applied_us = None;
            self.exposure_clamp = None;
        }
        if let Some(milli) = gain_milli {
            self.gain_requested_milli = milli;
            self.gain_clamp = None;
        }
        for (lane, asked) in asked.into_iter().enumerate() {
            if let Some((frame, code)) = asked {
                self.lanes[lane].insert(frame, code);
            }
        }
        self.post_due(sensor, at)?;
        // A value posted now for the current frame (a delay of zero) is in the report of that frame.
        self.read_report(sensor, at)?;
        Ok(landings)
    }

    /// Posts what is due in frame `at`: for each control the latest request whose post frame has
    /// come. Controls due in the same frame with the same frame and delay share one post, so they
    /// land together; controls with different delays are posted in their own frames.
    fn post_due<S: CameraSensor + ?Sized>(&mut self, sensor: &mut S, at: u64) -> Result<(), Error> {
        let mut due: [Option<(u64, u32)>; LANES] = [None; LANES];
        for (lane, slot) in self.lanes.iter_mut().zip(due.iter_mut()) {
            *slot = lane.take_due(at);
        }
        let mut sent = [false; LANES];
        for i in 0..LANES {
            let Some((frame, _)) = due[i] else { continue };
            if sent[i] {
                continue;
            }
            let delay = self.lanes[i].delay;
            let mut picked: [Option<u32>; LANES] = [None; LANES];
            for j in i..LANES {
                if let Some((f, code)) = due[j]
                    && !sent[j]
                    && f == frame
                    && self.lanes[j].delay == delay
                {
                    picked[j] = Some(code);
                    sent[j] = true;
                }
            }
            let controls = Controls {
                exposure: picked[EXPOSURE],
                analogue_gain: picked[GAIN],
                digital_gain: None,
                frame_length: picked[FRAME_LENGTH],
            };
            sensor
                .post_values(FrameValues {
                    // Opaque to Lemnos: the post frame is kept here as a record, not as the landing.
                    identity: frame,
                    controls,
                })
                .map_err(Error::Sensor)?;
            for (lane, code) in self.lanes.iter_mut().zip(picked) {
                if let Some(code) = code {
                    lane.record(at + u64::from(delay), code);
                }
            }
        }
        Ok(())
    }

    /// The controls in force on `frame`, and what the frame measured. Timestamp is the frame's
    /// `CLOCK_MONOTONIC` nanoseconds. For a frame the driver reported at its start, the values are
    /// that report; otherwise (no report, or a frame not yet started) they come from what was posted
    /// and the delays.
    pub fn metadata(&self, frame: u64, sensor_timestamp_ns: u64) -> FrameMetadata {
        let (exposure_code, gain_code, fl) = (
            self.lanes[EXPOSURE].value_at(frame),
            self.lanes[GAIN].value_at(frame),
            self.lanes[FRAME_LENGTH].value_at(frame),
        );
        let report = self
            .reports
            .iter()
            .rev()
            .find(|&&(f, _)| f == frame)
            .map(|&(_, in_force)| in_force);
        let (exposure_us, gain_milli, fl) = match report {
            Some(in_force) => (
                in_force.exposure_lines.map_or_else(
                    || self.limits.exposure_us(exposure_code),
                    |lines| self.limits.exposure_us_of_lines(lines),
                ),
                in_force.analogue_gain.map_or_else(
                    || self.limits.gain_milli(gain_code),
                    |code| self.limits.gain_milli(code),
                ),
                in_force.frame_length.unwrap_or(fl),
            ),
            None => (
                self.limits.exposure_us(exposure_code),
                self.limits.gain_milli(gain_code),
                fl,
            ),
        };
        let mut controls = [ControlValue::Auto; Control::COUNT];
        for control in Control::ALL {
            controls[control.index()] = control.spec().default;
        }
        controls[Control::Exposure.index()] = ControlValue::Manual(exposure_us);
        controls[Control::Gain.index()] = ControlValue::Manual(gain_milli);
        controls[Control::FrameRate.index()] = ControlValue::Manual(self.limits.rate_of(fl));
        FrameMetadata {
            controls,
            sensor_timestamp_ns,
            exposure_us,
            analogue_gain_milli: gain_milli,
            digital_gain_milli: DIGITAL_GAIN_MILLI,
            lux: None,
            colour_temperature_k: None,
            achieved_fps_milli: self.limits.rate_of(fl),
        }
    }

    /// The last frame started.
    pub fn frame(&self) -> Option<u64> {
        self.frame
    }

    /// Requests replaced or superseded by a later one for the same control (not posted).
    pub fn superseded(&self) -> u64 {
        self.lanes.iter().map(Lane::superseded).sum()
    }

    /// Requests dropped because a control had too many waiting.
    pub fn dropped(&self) -> u64 {
        self.lanes.iter().map(Lane::dropped).sum()
    }

    /// Requests waiting for their post frame.
    pub fn pending(&self) -> usize {
        self.lanes.iter().map(Lane::pending).sum()
    }
}
