//! The native controls of one camera: what is posted, when, and what is in force.

use lemnos_contract_camera_sensor::{CameraSensor, Controls, FrameValues, GainRange, Mode};
use styx_controls::{Control, ControlValue, FrameMetadata, Update};

use crate::error::{Error, Limit};
use crate::gain::{self, GainLaw};
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

/// Where a value was asked to land, and where it is predicted to land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Landing {
    /// The control.
    pub control: Control,
    /// The frame the value was asked to apply from.
    pub requested: u64,
    /// The first frame predicted to show it. Equal to `requested` when the request was on time.
    pub frame: u64,
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
}

/// The sensor's limits for one mode: what the contract's mode says, in the sensor's own codes.
#[derive(Debug, Clone, Copy)]
struct Limits {
    line_time_ps: u64,
    fraction_bits: u8,
    exposure_min: u32,
    exposure_margin: u32,
    gain: GainRange,
    law: GainLaw,
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

    /// Thousandths of gain for a code. Only called with a code this crate made, under a law that
    /// [`NativeControls::new`] has already accepted, so the conversion cannot be refused.
    fn gain_milli(&self, code: u32) -> u32 {
        gain::code_to_gain(self.law, &self.gain, code).unwrap_or(0)
    }

    fn frame_rate(&self, lines: u32) -> u32 {
        units::frame_rate_milli(u64::from(lines), self.line_time_ps)
    }

    /// The frame length in lines for a frame rate, if the sensor takes it.
    fn frame_length(&self, mhz: u32) -> Result<u32, Error> {
        let limit = || Limit {
            control: Control::FrameRate,
            requested: mhz,
            min: self.frame_rate(self.fl_max),
            max: self.frame_rate(self.fl_min),
            frame_lines: None,
        };
        let lines =
            units::frame_length(mhz, self.line_time_ps).ok_or_else(|| Error::Limit(limit()))?;
        if lines < u64::from(self.fl_min) || lines > u64::from(self.fl_max) {
            return Err(Error::Limit(limit()));
        }
        Ok(lines as u32)
    }

    /// The exposure code for `us` at a frame of `fl` lines, if the sensor takes it there.
    fn exposure(&self, us: u32, fl: u32) -> Result<u32, Error> {
        let code = units::exposure_code(us, self.line_time_ps, self.fraction_bits);
        let max = units::exposure_max_code(u64::from(fl), self.exposure_margin, self.fraction_bits);
        if code < u64::from(self.exposure_min) || code > max {
            return Err(Error::Limit(Limit {
                control: Control::Exposure,
                requested: us,
                min: self.exposure_us(self.exposure_min),
                max: units::exposure_us(max, self.line_time_ps, self.fraction_bits),
                frame_lines: Some(fl),
            }));
        }
        Ok(code as u32)
    }

    /// The largest exposure, in microseconds, a frame of `fl` lines allows.
    fn exposure_max_us(&self, fl: u32) -> u32 {
        let max = units::exposure_max_code(u64::from(fl), self.exposure_margin, self.fraction_bits);
        units::exposure_us(max, self.line_time_ps, self.fraction_bits)
    }

    /// The gain code for `milli`, if the sensor has that code.
    fn gain(&self, milli: u32) -> Result<u32, Error> {
        let code = gain::gain_to_code(self.law, &self.gain, milli)?;
        if code < self.gain.min || code > self.gain.max {
            return Err(Error::Limit(Limit {
                control: Control::Gain,
                requested: milli,
                min: self.gain_milli(self.gain.min),
                max: self.gain_milli(self.gain.max),
                frame_lines: None,
            }));
        }
        Ok(code)
    }
}

impl NativeControls {
    /// The controls of a sensor in `mode`, starting at `initial`, with gain converted by `law`.
    /// Refuses an initial value the sensor cannot take, a mode with no line time or no gain unity
    /// code, and [`GainLaw::Unknown`] ([`Error::GainLaw`]): no gain code is assumed.
    pub fn new(mode: &Mode, initial: Initial, law: GainLaw) -> Result<Self, Error> {
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
            law,
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
        let fl = limits.frame_length(initial.frame_rate_milli)?;
        let exposure = limits.exposure(initial.exposure_us, fl)?;
        let gain = limits.gain(initial.gain_milli)?;
        Ok(Self {
            limits,
            lanes: [
                Lane::new(mode.exposure_delay.into(), exposure),
                Lane::new(mode.gain_delay.into(), gain),
                Lane::new(mode.frame_length_delay.into(), fl),
            ],
            frame: None,
        })
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
    /// Refused, with nothing posted: a control with no sensor value here, a value outside the
    /// generic range or this sensor's limits (including an exposure too long for the frame length
    /// in force on its frame, and a frame rate that would make the exposure in force too long).
    pub fn request<S: CameraSensor + ?Sized>(
        &mut self,
        sensor: &mut S,
        updates: &[Update],
    ) -> Result<Landings, Error> {
        let at = self.frame.ok_or(Error::NotStreaming)?;
        let mut asked: [Option<(u64, u32)>; LANES] = [None; LANES];
        let mut exposure_us: Option<(u64, u32)> = None;
        let mut landings = Landings::default();
        for update in updates {
            let control = update.control;
            let lane = lane_of(control).ok_or(Error::Unsupported(control))?;
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
                GAIN => asked[GAIN] = Some((frame, self.limits.gain(value)?)),
                _ => asked[FRAME_LENGTH] = Some((frame, self.limits.frame_length(value)?)),
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
            });
            landings.len += 1;
        }

        // Exposure is checked against the frame length in force on its frame, which may be the one
        // this request sets. A frame length is checked against the exposure in force on its frame,
        // which may be the one this request sets.
        let fl_asked = asked[FRAME_LENGTH];
        if let Some((frame, us)) = exposure_us {
            let fl = self.lanes[FRAME_LENGTH].planned_at(frame, fl_asked);
            asked[EXPOSURE] = Some((frame, self.limits.exposure(us, fl)?));
        }
        if let Some((frame, fl)) = fl_asked {
            let exposure = self.lanes[EXPOSURE].planned_at(frame, asked[EXPOSURE]);
            let max = units::exposure_max_code(
                u64::from(fl),
                self.limits.exposure_margin,
                self.limits.fraction_bits,
            );
            if u64::from(exposure) > max {
                return Err(Error::Limit(Limit {
                    control: Control::Exposure,
                    requested: self.limits.exposure_us(exposure),
                    min: self.limits.exposure_us(self.limits.exposure_min),
                    max: self.limits.exposure_max_us(fl),
                    frame_lines: Some(fl),
                }));
            }
        }

        for (lane, asked) in asked.into_iter().enumerate() {
            if let Some((frame, code)) = asked {
                self.lanes[lane].insert(frame, code);
            }
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
    /// frame: `camera.sensor/1` has no per-frame read-back at the lock (Lemnos J4 adds it).
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
        controls[Control::FrameRate.index()] = ControlValue::Manual(self.limits.frame_rate(fl));
        FrameMetadata {
            controls,
            sensor_timestamp_ns,
            exposure_us,
            analogue_gain_milli: gain_milli,
            digital_gain_milli: DIGITAL_GAIN_MILLI,
            lux: None,
            colour_temperature_k: None,
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
