//! The native controls of one camera: what is posted, when, and what is in force.

use lemnos_contract_camera_sensor::{CameraSensor, Controls, FrameValues, GainRange, Mode};
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
    /// line, or the nearest rate the sensor's frame lengths reach when the ask is outside them.
    pub achieved_milli: u32,
    /// The rate asked for is outside the sensor's frame-length limits, so the frame length is the
    /// nearest limit. The achieved rate says what the sensor runs at.
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
    /// For a frame rate: the rate the frame length gives, in millihertz, which is what the frames
    /// will run at. `None` for other controls.
    pub achieved_milli: Option<u32>,
    /// For a frame rate: the request was clamped to the sensor's frame-length limits.
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
    initial_rate: FrameRate,
    /// The exposure asked for last, in microseconds.
    exposure_requested_us: u32,
    /// The exposure the last decision on exposure applied, in microseconds.
    exposure_applied_us: u32,
    /// The clamp of the last decision on exposure, if it was clamped.
    exposure_clamp: Option<Clamp>,
    /// The clamp of the last gain request, if it was clamped.
    gain_clamp: Option<Clamp>,
}

/// A value fitted to the sensor's range: its code, its value in the control's unit, and the end it
/// was moved to, if it was moved.
#[derive(Debug, Clone, Copy)]
struct Fitted {
    code: u32,
    applied: u32,
    bound: Option<ClampReason>,
}

impl Fitted {
    /// The clamp for a request of `requested`, if this fit moved it.
    fn clamp(self, control: Control, requested: u32, frame_lines: Option<u32>) -> Option<Clamp> {
        self.bound
            .map(|reason| Clamp::new(control, requested, self.applied, frame_lines, reason))
    }
}

/// The sensor's limits for one mode: what the contract's mode says, in the sensor's own codes.
#[derive(Debug, Clone, Copy)]
struct Limits {
    line_time_ps: u64,
    fraction_bits: u8,
    exposure_min: u32,
    exposure_margin: u32,
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
        units::exposure_us(u64::from(code), self.line_time_ps, self.fraction_bits)
    }

    /// Thousandths of gain for a code. Only called with a code this crate made, under the range's
    /// law that [`NativeControls::new`] has already converted, so the conversion cannot be refused.
    fn gain_milli(&self, code: u32) -> u32 {
        gain::code_to_gain(&self.gain, code).unwrap_or(0)
    }

    /// The rate a frame of `lines` lines runs at, in millihertz.
    fn rate_of(&self, lines: u32) -> u32 {
        units::frame_rate_milli(u64::from(lines), self.line_time_ps)
    }

    /// The frame length for a frame rate, in the sensor's frame lengths. The length is the nearest
    /// whole line to the rate; a rate outside the sensor's frame lengths is clamped to the nearest
    /// one, and the rate it runs at is reported. Only a rate of zero is refused.
    fn frame_rate(&self, mhz: u32) -> Result<FrameRate, Error> {
        let lines = units::frame_length(mhz, self.line_time_ps).ok_or_else(|| {
            Error::Limit(Limit {
                control: Control::FrameRate,
                requested: mhz,
                min: self.rate_of(self.fl_max),
                max: self.rate_of(self.fl_min),
                frame_lines: None,
            })
        })?;
        let (min, max) = (u64::from(self.fl_min), u64::from(self.fl_max));
        let clamped = lines < min || lines > max;
        let lines = lines.clamp(min, max) as u32;
        Ok(FrameRate {
            lines,
            achieved_milli: self.rate_of(lines),
            clamped,
        })
    }

    /// The exposure codes the sensor takes at a frame of `fl` lines: its minimum, and the ceiling the
    /// frame allows (the frame less the mode's margin). A frame too short for the margin gives the
    /// minimum as its ceiling, so the range is never empty.
    fn exposure_bounds(&self, fl: u32) -> (u64, u64) {
        let min = u64::from(self.exposure_min);
        let max = units::exposure_max_code(u64::from(fl), self.exposure_margin, self.fraction_bits);
        (min, max.max(min))
    }

    /// The exposure for `us` at a frame of `fl` lines, moved to the nearest code the sensor takes
    /// there. A request moved to a bound by rounding alone, with the same microseconds, is not a
    /// clamp.
    fn exposure(&self, us: u32, fl: u32) -> Fitted {
        let code = units::exposure_code(us, self.line_time_ps, self.fraction_bits);
        let (min, max) = self.exposure_bounds(fl);
        let (code, bound) = if code > max {
            (max, Some(ClampReason::Ceiling))
        } else if code < min {
            (min, Some(ClampReason::Floor))
        } else {
            (code, None)
        };
        let code = u32::try_from(code).unwrap_or(u32::MAX);
        let applied = self.exposure_us(code);
        Fitted {
            code,
            applied,
            bound: bound.filter(|_| applied != us),
        }
    }

    /// The gain for `milli`, moved to the nearest gain of the sensor's codes. The codes are the
    /// range's `min`..=`max`; a gain outside them is moved to the end it is past.
    fn gain(&self, milli: u32) -> Result<Fitted, Error> {
        let (low, high) = (
            self.gain_milli(self.gain.min),
            self.gain_milli(self.gain.max),
        );
        let (wanted, bound) = if milli < low {
            (low, Some(ClampReason::Floor))
        } else if milli > high {
            (high, Some(ClampReason::Ceiling))
        } else {
            (milli, None)
        };
        let code = gain::gain_to_code(&self.gain, wanted)?.clamp(self.gain.min, self.gain.max);
        let applied = self.gain_milli(code);
        Ok(Fitted {
            code,
            applied,
            bound: bound.filter(|_| applied != milli),
        })
    }
}

impl NativeControls {
    /// The controls of a sensor in `mode`, starting at `initial`. Gain converts by the mode's own
    /// gain law (the contract's `GainRange::law`). A frame rate outside the sensor's frame lengths
    /// is clamped and reported ([`NativeControls::initial_rate`]). Refuses an initial value the
    /// sensor cannot take, and a mode with no line time or no gain unity code.
    pub fn new(mode: &Mode, initial: Initial) -> Result<Self, Error> {
        if mode.line_time_ps == 0 {
            return Err(Error::Mode("line_time_ps is zero"));
        }
        if mode.analogue_gain.unity == 0 {
            return Err(Error::Mode("analogue gain has no unity code"));
        }
        let limits = Limits {
            line_time_ps: u64::from(mode.line_time_ps),
            fraction_bits: mode.exposure_fraction_bits,
            exposure_min: mode.exposure_min,
            exposure_margin: mode.exposure_margin,
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
        let fl = rate.lines;
        // The frame rate has priority: the exposure is fitted to the frame length it gives.
        let exposure = limits.exposure(initial.exposure_us, fl);
        let gain = limits.gain(initial.gain_milli)?;
        Ok(Self {
            lanes: [
                Lane::new(mode.exposure_delay.into(), exposure.code),
                Lane::new(mode.gain_delay.into(), gain.code),
                Lane::new(mode.frame_length_delay.into(), fl),
            ],
            frame: None,
            initial_rate: rate,
            exposure_requested_us: initial.exposure_us,
            exposure_applied_us: exposure.applied,
            exposure_clamp: exposure.clamp(Control::Exposure, initial.exposure_us, Some(fl)),
            gain_clamp: gain.clamp(Control::Gain, initial.gain_milli, None),
            limits,
        })
    }

    /// Exposure as it stands: the value asked for last, the value applied, and the clamp of the
    /// last decision on it (`None` when that decision was not clamped).
    pub fn exposure_report(&self) -> ExposureReport {
        ExposureReport {
            requested_us: self.exposure_requested_us,
            applied_us: self.exposure_applied_us,
            clamp: self.exposure_clamp.clone(),
        }
    }

    /// The clamp of the last decision on `control` (exposure or gain), if that decision was
    /// clamped. `None` for the other controls, and when it was not.
    pub fn clamp(&self, control: Control) -> Option<&Clamp> {
        match control {
            Control::Exposure => self.exposure_clamp.as_ref(),
            Control::Gain => self.gain_clamp.as_ref(),
            _ => None,
        }
    }

    /// The frame rate the initial value gave: the frame length and the rate it runs at, and whether
    /// the sensor's limits clamped the ask.
    pub fn initial_rate(&self) -> FrameRate {
        self.initial_rate
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

    /// Frame `frame` has started (the frame ids of the frame stream). Posts what is due in it: each
    /// value whose frame is `frame + delay`, grouped by frame and delay so one post holds values
    /// that land together.
    pub fn frame_start<S: CameraSensor + ?Sized>(
        &mut self,
        sensor: &mut S,
        frame: u64,
    ) -> Result<(), Error> {
        self.frame = Some(frame);
        self.post_due(sensor, frame)
    }

    /// Asks for values from their frames on. Each is posted now if its post frame has come (the
    /// current frame or earlier), and otherwise in the frame that is its delay before it. Returns
    /// where each is predicted to land.
    ///
    /// A value the sensor cannot take exactly is moved to the nearest one it can take, and the
    /// clamp says why ([`NativeControls::clamp`], [`NativeControls::exposure_report`]): an exposure
    /// past its frame length's ceiling or the minimum, a gain past the sensor's codes, and an
    /// exposure in force that a shorter frame cannot hold (the frame rate has priority, so the
    /// frame is never lengthened for an exposure).
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
        let mut exposure_us: Option<(u64, u32)> = None;
        let mut gain_fit: Option<(u32, Fitted)> = None;
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
                EXPOSURE => exposure_us = Some((frame, value)),
                GAIN => {
                    let fit = self.limits.gain(value)?;
                    asked[GAIN] = Some((frame, fit.code));
                    gain_fit = Some((value, fit));
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
                clamped: rate.is_some_and(|r| r.clamped),
            });
            landings.len += 1;
        }

        // Exposure is fitted to the frame length in force on its frame, which may be the one this
        // request sets. Then a frame length is checked against the exposure in force on its frame,
        // which may be the one this request sets: the frame rate has priority, so the exposure is
        // moved to the frame's ceiling and the frame length is kept.
        let fl_asked = asked[FRAME_LENGTH];
        let requested_us = exposure_us.map_or(self.exposure_requested_us, |(_, us)| us);
        let mut exposure_clamp = None;
        if let Some((frame, us)) = exposure_us {
            let fl = self.lanes[FRAME_LENGTH].planned_at(frame, fl_asked);
            let fit = self.limits.exposure(us, fl);
            asked[EXPOSURE] = Some((frame, fit.code));
            exposure_clamp = fit.clamp(Control::Exposure, us, Some(fl));
        }
        if let Some((frame, fl)) = fl_asked {
            let planned = self.lanes[EXPOSURE].planned_at(frame, asked[EXPOSURE]);
            let (_, max) = self.limits.exposure_bounds(fl);
            if u64::from(planned) > max {
                let code = u32::try_from(max).unwrap_or(u32::MAX);
                let applied = self.limits.exposure_us(code);
                asked[EXPOSURE] = Some((frame, code));
                exposure_clamp = (applied != requested_us).then(|| {
                    Clamp::new(
                        Control::Exposure,
                        requested_us,
                        applied,
                        Some(fl),
                        ClampReason::Ceiling,
                    )
                });
            }
        }
        let exposure_decided = exposure_us.is_some() || fl_asked.is_some();
        let applied_us = match exposure_us.map(|(f, _)| f).or(fl_asked.map(|(f, _)| f)) {
            Some(frame) if exposure_decided => {
                let code = self.lanes[EXPOSURE].planned_at(frame, asked[EXPOSURE]);
                self.limits.exposure_us(code)
            }
            _ => self.exposure_applied_us,
        };

        for (lane, asked) in asked.into_iter().enumerate() {
            if let Some((frame, code)) = asked {
                self.lanes[lane].insert(frame, code);
            }
        }
        if exposure_us.is_some() {
            self.exposure_requested_us = requested_us;
        }
        if exposure_decided {
            self.exposure_applied_us = applied_us;
            self.exposure_clamp = exposure_clamp;
        }
        if let Some((requested, fit)) = gain_fit {
            self.gain_clamp = fit.clamp(Control::Gain, requested, None);
        }
        self.post_due(sensor, at)?;
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
    /// `CLOCK_MONOTONIC` nanoseconds. Values come from what was posted and the delays, not from the
    /// frame: the sensor's embedded data (`SensorInfo::embedded_data`) is not read back here yet.
    ///
    /// On a free-running sensor a value may land one frame later than recorded here: the write
    /// frame is taken as the last frame delivered, and the sensor may already be exposing the
    /// next one. Until read-back exists, treat the recorded frame as the earliest it can land.
    pub fn metadata(&self, frame: u64, sensor_timestamp_ns: u64) -> FrameMetadata {
        let exposure = self.lanes[EXPOSURE].value_at(frame);
        let gain = self.lanes[GAIN].value_at(frame);
        let fl = self.lanes[FRAME_LENGTH].value_at(frame);
        let exposure_us = self.limits.exposure_us(exposure);
        let gain_milli = self.limits.gain_milli(gain);
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
