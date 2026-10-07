//! The sensor side of a camera: the driver plus the per-stream bookkeeping around it. Bring-up
//! sequencing (power, chip id, stream-off, init, mode), serving the receiver's start and stop,
//! frame starts, typed control requests (immediate writes within the current frame), the
//! values that produced each frame, embedded data reports, the lens, and shut-down.
//!
//! It is generic over the register bus and pins, so it runs over `styx-sensor`'s
//! [`MockBus`](styx_sensor::MockBus) in tests exactly as over I²C on a device.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::time::Duration;

use styx_hal::ErrorKind;
use styx_sensor::{
    AppliedControls, ControlRequest, ControlSet, DriverBus, DriverState, Landing, Landings,
    Mismatch, RegWrite, SensorDescription, SensorDriver, SensorPins, Step, Timing,
};

use crate::error::{Error, Result};
use crate::lens::{LensControl, PdafFrames};

/// The platform's monotonic clock (`CLOCK_MONOTONIC` on Linux: the clock of V4L2 buffer and
/// event timestamps), as a duration since an arbitrary origin.
pub type Clock = fn() -> Duration;

/// The typed values that produced a frame, as delivered with it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameControls {
    /// Exposure time.
    pub exposure: Duration,
    /// Exposure in lines.
    pub exposure_lines: f64,
    /// Analogue gain.
    pub analog_gain: f64,
    /// Digital gain (1 without one).
    pub digital_gain: f64,
    /// Frame length in lines.
    pub frame_length: u32,
    /// Frame duration (the period this frame was read out with).
    pub frame_duration: Duration,
    /// Some values were read back from the frame (embedded data) rather than predicted.
    pub verified: bool,
    /// Where the focus lens was (cameras with a lens; predicted from the moves written).
    pub lens: Option<styx_sensor::LensFrame>,
}

impl FrameControls {
    /// Total gain (analogue × digital).
    pub fn gain(&self) -> f64 {
        self.analog_gain * self.digital_gain
    }
}

impl From<&AppliedControls> for FrameControls {
    fn from(a: &AppliedControls) -> Self {
        FrameControls {
            exposure: a.exposure,
            exposure_lines: a.exposure_lines,
            analog_gain: a.analog_gain,
            digital_gain: a.digital_gain,
            frame_length: a.frame_length,
            frame_duration: a.frame_duration,
            verified: !a.codes.reported.is_empty(),
            lens: None,
        }
    }
}

/// The format a receiver's start request must carry for the configuration set up (a receiver
/// that asks for the sensor start with the format it was configured for, as the Styx sensor
/// bridge does).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartFormat {
    /// Media bus code.
    pub code: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Link frequency in Hz.
    pub link_freq: i64,
}

impl StartFormat {
    /// Whether a start request carrying `got` matches.
    pub fn check(&self, got: &StartFormat) -> core::result::Result<(), String> {
        let g = (got.code, got.width, got.height, got.link_freq);
        let exp = (self.code, self.width, self.height, self.link_freq);
        if g == exp {
            Ok(())
        } else {
            Err(format!("start request carries {g:?}, configured {exp:?}"))
        }
    }
}

/// Why a start or stop was not served: the kind ([`ErrorKind::InvalidConfig`] for a start
/// request that does not match the configuration, [`ErrorKind::Io`] when the sensor did not
/// take the writes) and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServeError {
    /// What kind of failure.
    pub kind: ErrorKind,
    /// Why.
    pub why: String,
}

/// Problems that would let the sensor stream (drive its lanes out of LP-11) before the
/// receiver's start: a write before `stream_on` of a `stream_on` register value, or a software
/// reset (`0x0103`).
pub fn standby_problems(desc: &SensorDescription, mode: &str, format: &str) -> Vec<String> {
    fn writes(steps: &[Step]) -> impl Iterator<Item = &RegWrite> {
        steps.iter().filter_map(Step::as_write)
    }
    let mut problems = Vec::new();
    let on: Vec<&RegWrite> = writes(&desc.sequences.stream_on).collect();
    if on.is_empty() {
        problems.push("stream_on has no register writes".to_string());
    }
    let mut check = |what: &str, steps: &[Step]| {
        for w in writes(steps) {
            if on
                .iter()
                .any(|s| s.address == w.address && s.value == w.value)
            {
                let w = styx_sensor::show_write(w);
                problems.push(format!("{what} writes {w}, the stream-on value"));
            }
            if w.address == 0x0103 && w.value & 1 != 0 {
                let w = styx_sensor::show_write(w);
                problems.push(format!("{what} writes {w} (software reset)"));
            }
        }
    };
    check("power_up", &desc.sequences.power_up);
    check("init", &desc.sequences.init);
    match desc.mode(mode) {
        Ok(m) => {
            check("mode registers", &m.registers);
            match desc.format_for(m, format) {
                Ok(f) => check("format registers", &f.registers),
                Err(e) => problems.push(e.to_string()),
            }
        }
        Err(e) => problems.push(e.to_string()),
    }
    problems
}

/// Where the last [`SensorState::bring_up`] spent its time (zero for steps it skipped, and
/// without a clock).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BringUpTimes {
    /// The power-up sequence (supplies, clock, reset, settle delays, its register writes).
    pub power_up: Duration,
    /// Reading and checking the chip id.
    pub chip_id: Duration,
    /// The common init registers.
    pub init: Duration,
    /// Format and mode registers, line and frame length, initial controls, flips.
    pub mode: Duration,
}

/// How long before a frame ends [`SensorState::request_at_now`] must have written by default:
/// the bus transfers (a group hold of three controls is about seven at 100 kHz) and slack.
pub const DEFAULT_WRITE_MARGIN: Duration = Duration::from_millis(4);

/// The sensor driver plus the per-stream bookkeeping around it.
#[derive(Debug)]
pub struct SensorState<B, P> {
    driver: SensorDriver<B, P>,
    last_start: Option<u64>,
    expected: Option<StartFormat>,
    frame_starts: u64,
    starts_served: u64,
    bring_up_times: BringUpTimes,
    /// When the last frame started (platform clock), if known.
    last_start_at: Option<Duration>,
    /// Immediate writes: how long before the current frame ends a write must be done.
    write_margin: Option<Duration>,
    clock: Option<Clock>,
    lens: Option<LensControl>,
    pdaf: PdafFrames,
}

impl<B: DriverBus, P: SensorPins> SensorState<B, P> {
    /// Wraps a driver (no clock: see [`Self::with_clock`]).
    pub fn new(driver: SensorDriver<B, P>) -> Self {
        Self {
            driver,
            last_start: None,
            expected: None,
            frame_starts: 0,
            starts_served: 0,
            bring_up_times: BringUpTimes::default(),
            last_start_at: None,
            write_margin: Some(DEFAULT_WRITE_MARGIN),
            clock: None,
            lens: None,
            pdaf: PdafFrames::default(),
        }
    }

    /// Uses `clock` for bring-up times, immediate writes and lens moves. Without a clock every
    /// request waits for a frame start and frames carry no lens motion.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// The clock, if any.
    pub fn clock(&self) -> Option<Clock> {
        self.clock
    }

    /// The clock's time (zero without one).
    pub fn now(&self) -> Duration {
        self.clock.map_or(Duration::ZERO, |c| c())
    }

    /// Lets [`Self::request_at_now`] write in the current frame when at least `margin` of it
    /// is left (`None`: never; every request then waits for the next frame start).
    pub fn set_write_margin(&mut self, margin: Option<Duration>) {
        self.write_margin = margin;
    }

    /// Whether requested values wait for a coming frame start to be written (while
    /// streaming).
    pub fn writes_pending(&self) -> bool {
        self.driver.state() == DriverState::Streaming
            && self.driver.scheduler().is_some_and(|s| s.has_pending())
    }

    /// How much of the current frame is left at `now` (platform clock): `None` when not
    /// streaming or when the frame's start time is not known.
    pub fn frame_time_left(&self, now: Duration) -> Option<Duration> {
        if self.driver.state() != DriverState::Streaming {
            return None;
        }
        let (seq, at) = (self.last_start?, self.last_start_at?);
        let end = at + self.applied(seq)?.frame_duration;
        Some(end.saturating_sub(now))
    }

    /// Where the last [`Self::bring_up`] spent its time.
    pub fn bring_up_times(&self) -> BringUpTimes {
        self.bring_up_times
    }

    /// The driver.
    pub fn driver(&self) -> &SensorDriver<B, P> {
        &self.driver
    }

    /// The driver, mutably.
    pub fn driver_mut(&mut self) -> &mut SensorDriver<B, P> {
        &mut self.driver
    }

    /// Timing of the active mode.
    pub fn timing(&self) -> Option<Timing> {
        self.driver.mode().map(|m| m.timing)
    }

    fn elapsed(&self, since: Duration) -> Duration {
        self.now().saturating_sub(since)
    }

    /// Powers the sensor, checks its chip id, writes init and the mode. The sensor stays in
    /// software standby. Powers up only if off; an active mode is replaced, except that a
    /// powered sensor already in this mode at its default line length (a camera kept warm
    /// between sessions) is left as it is: its registers have not changed in standby.
    pub fn bring_up(&mut self, mode: &str, format: &str) -> Result<()> {
        let problems = if self.driver.is_kernel() {
            // The kernel driver starts and stops the sensor itself.
            Vec::new()
        } else {
            standby_problems(self.driver.description(), mode, format)
        };
        if !problems.is_empty() {
            return Err(Error::InvalidConfig(format!(
                "the description would leave standby before the start event: {}",
                problems.join("; ")
            )));
        }
        let mut times = BringUpTimes::default();
        let t = self.now();
        match self.driver.state() {
            DriverState::Streaming => return Err(Error::State("sensor is streaming")),
            DriverState::Off => {
                self.driver.power_up()?;
                times.power_up = self.elapsed(t);
                let t = self.now();
                let checked = self.driver.verify_chip_id();
                times.chip_id = self.elapsed(t);
                if let Err(e) = checked {
                    let _ = self.driver.power_down();
                    return Err(e.into());
                }
                // "Off" is only what this process did: a sensor whose supplies cannot be cut
                // (digital rails always on) keeps its registers, and one whose previous owner
                // was killed mid-stream is still streaming, its lanes in HS. The receiver then
                // never sees the LP-11 → HS start of a frame, and the first stream gets no
                // frames. Stop it before anything else.
                if let Err(e) = self.write_stream_off() {
                    let _ = self.driver.power_down();
                    return Err(Error::Bus {
                        during: "writing stream_off",
                        source: e,
                    });
                }
                let t = self.now();
                if let Err(e) = self.driver.init() {
                    let _ = self.driver.power_down();
                    return Err(e.into());
                }
                times.init = self.elapsed(t);
            }
            DriverState::Powered => {}
        }
        let t = self.now();
        let desc = self.driver.description();
        let same = self.driver.mode().is_some_and(|m| {
            m.mode == mode
                && m.format == format
                && desc
                    .mode(mode)
                    .is_ok_and(|d| m.timing.hblank == d.hblank.default)
        });
        if !same {
            self.driver.set_mode(mode, format)?;
        }
        times.mode = self.elapsed(t);
        self.bring_up_times = times;
        self.last_start = None;
        Ok(())
    }

    /// Sets what the next start request must carry.
    pub fn expect_start(&mut self, expected: StartFormat) {
        self.expected = Some(expected);
    }

    /// What the next start request must carry, if set.
    pub fn expected_start(&self) -> Option<StartFormat> {
        self.expected
    }

    /// Serves the receiver's start request (carrying `format`, when the receiver says what it
    /// was configured for): writes stream-on and starts the control schedule. A failed start is
    /// undone (best effort).
    pub fn serve_start(
        &mut self,
        format: Option<&StartFormat>,
    ) -> core::result::Result<(), ServeError> {
        if let (Some(exp), Some(got)) = (self.expected, format)
            && let Err(why) = exp.check(got)
        {
            return Err(ServeError {
                kind: ErrorKind::InvalidConfig,
                why,
            });
        }
        self.last_start = None;
        if let Err(e) = self.driver.start_streaming() {
            // Whatever part of stream-on went through is undone (best effort).
            let _ = self.write_stream_off();
            return Err(ServeError {
                kind: ErrorKind::Io,
                why: format!("starting the sensor: {e}"),
            });
        }
        self.starts_served += 1;
        Ok(())
    }

    /// Serves the receiver's stop request: writes stream-off (nothing to do when not
    /// streaming).
    pub fn serve_stop(&mut self) -> core::result::Result<(), ServeError> {
        self.last_start = None;
        if self.driver.state() != DriverState::Streaming {
            return Ok(());
        }
        self.driver.stop_streaming().map_err(|e| ServeError {
            kind: ErrorKind::Io,
            why: format!("stopping the sensor: {e}"),
        })
    }

    /// Starts the sensor's control schedule without a start request: for a sensor whose
    /// streaming the receiver starts itself (a Linux kernel driver), just before it does
    /// (frame 0's values are set first; the driver applies them as it starts streaming).
    pub fn start_streaming(&mut self) -> Result<()> {
        self.last_start = None;
        self.driver.start_streaming()?;
        self.starts_served += 1;
        Ok(())
    }

    /// Puts the sensor back in standby if it streams (best effort).
    pub fn standby(&mut self) {
        if self.driver.state() == DriverState::Streaming {
            let _ = self.driver.stop_streaming();
        }
    }

    /// Start requests served successfully.
    pub fn starts_served(&self) -> u64 {
        self.starts_served
    }

    /// Frame `seq` started: writes what is due. Repeated or older sequences are ignored.
    /// Returns the controls written.
    pub fn frame_start(&mut self, seq: u64) -> Result<ControlSet> {
        self.frame_start_at(seq, None)
    }

    /// [`Self::frame_start`], with when the frame started (platform clock, e.g. the frame-start
    /// event's timestamp), which lets [`Self::request_at_now`] write within it.
    pub fn frame_start_at(&mut self, seq: u64, at: Option<Duration>) -> Result<ControlSet> {
        if self.last_start.is_some_and(|l| seq <= l) {
            return Ok(ControlSet::new());
        }
        self.last_start = Some(seq);
        self.last_start_at = at;
        self.frame_starts += 1;
        if self.driver.state() != DriverState::Streaming {
            return Ok(ControlSet::new());
        }
        let set = self.driver.frame_start(seq)?.controls;
        self.lens_frame_start(seq, at)?;
        Ok(set)
    }

    /// The last frame start seen in this stream.
    pub fn current_frame(&self) -> Option<u64> {
        self.last_start
    }

    /// Frame starts seen since the camera was opened.
    pub fn frame_starts(&self) -> u64 {
        self.frame_starts
    }

    /// The first frame a new request can target: the frame after the current one (the
    /// scheduler moves it later when a control's delay needs that).
    pub fn next_frame(&self) -> u64 {
        match (self.driver.state(), self.last_start) {
            (DriverState::Streaming, Some(s)) => s + 1,
            _ => 0,
        }
    }

    /// Asks for typed values as soon as possible. Returns where each lands.
    pub fn request(&mut self, req: &ControlRequest) -> Result<Vec<Landing>> {
        let frame = self.next_frame();
        self.request_at(frame, req)
    }

    /// Asks for typed values from frame `frame` on.
    pub fn request_at(&mut self, frame: u64, req: &ControlRequest) -> Result<Vec<Landing>> {
        Ok(self.request_at_landings(frame, req)?.to_vec())
    }

    /// [`Self::request_at`] without allocating: the landings in a fixed list.
    pub fn request_at_landings(&mut self, frame: u64, req: &ControlRequest) -> Result<Landings> {
        Self::check(req)?;
        Ok(self.driver.request(frame, req)?)
    }

    /// [`Self::request_at`], writing at once what is due in the current frame when enough of
    /// it is left at `now` (platform clock; see [`Self::set_write_margin`]) instead of at the
    /// next frame start, so values land a frame earlier. Before streaming the values for frame
    /// 0 are written at once (not with the stream-on sequence).
    pub fn request_at_now(
        &mut self,
        frame: u64,
        req: &ControlRequest,
        now: Duration,
    ) -> Result<Vec<Landing>> {
        Ok(self.request_at_now_landings(frame, req, now)?.to_vec())
    }

    /// [`Self::request_at_now`] without allocating: the landings in a fixed list.
    pub fn request_at_now_landings(
        &mut self,
        frame: u64,
        req: &ControlRequest,
        now: Duration,
    ) -> Result<Landings> {
        Self::check(req)?;
        let in_time = match self.driver.state() {
            DriverState::Powered => true,
            DriverState::Streaming => self
                .write_margin
                .zip(self.frame_time_left(now))
                .is_some_and(|(margin, left)| left > margin),
            DriverState::Off => false,
        };
        if in_time {
            Ok(self.driver.request_now(frame, req)?)
        } else {
            Ok(self.driver.request(frame, req)?)
        }
    }

    fn check(req: &ControlRequest) -> Result<()> {
        if let Some(d) = req.frame_duration
            && d.is_zero()
        {
            return Err(Error::InvalidConfig("zero frame duration".into()));
        }
        if let Some(g) = req.gain
            && !(g.is_finite() && g > 0.0)
        {
            // The value only with std: formatting a float would bring core's float
            // formatting (about 8 KB) into a firmware image for this one message.
            #[cfg(feature = "std")]
            return Err(Error::InvalidConfig(format!("gain {g}")));
            #[cfg(not(feature = "std"))]
            return Err(Error::InvalidConfig("gain not finite and positive".into()));
        }
        Ok(())
    }

    /// The values that produced frame `seq` (predicted, or read back where reported).
    pub fn applied(&self, seq: u64) -> Option<FrameControls> {
        let f = self.driver.applied(seq).as_ref().map(FrameControls::from)?;
        Some(self.with_lens(seq, f))
    }

    /// Whether the sensor streams.
    pub fn is_streaming(&self) -> bool {
        self.driver.state() == DriverState::Streaming
    }

    /// The values predicted for the latest started frame (or frame 0 before streaming).
    pub fn latest(&self) -> Option<FrameControls> {
        self.applied(self.last_start.unwrap_or(0))
    }

    /// Records the control values read back from a frame's embedded data. Returns the values
    /// that differ from the prediction.
    pub fn report_embedded(&mut self, seq: u64, data: &[u8]) -> Result<Vec<Mismatch>> {
        self.decode_pdaf(seq, data);
        let codes = self.driver.description().decode_embedded(data);
        if codes.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.driver.report(seq, &codes)?.to_vec())
    }

    /// Writes the stream-off registers whatever the driver's state (a start that failed half
    /// way, a sensor left streaming).
    pub fn write_stream_off(&mut self) -> core::result::Result<(), styx_sensor::BusError> {
        let off: Vec<RegWrite> = self
            .driver
            .description()
            .sequences
            .stream_off
            .iter()
            .filter_map(Step::as_write)
            .copied()
            .collect();
        self.driver
            .bus_mut()
            .write_sequence(&off)
            .map_err(styx_sensor::BusError::from_register)
    }

    /// Puts the sensor back in standby and powers it down (whatever state it is in). Best
    /// effort: when the sensor does not answer (the stream-off writes fail), it is powered down
    /// anyway and the first error is returned.
    pub fn shut_down(&mut self) -> Result<()> {
        self.last_start = None;
        if let Some(l) = &mut self.lens {
            l.power_down();
        }
        let mut result: Result<()> = Ok(());
        if self.driver.state() == DriverState::Streaming
            && let Err(e) = self.driver.stop_streaming()
        {
            result = Err(e.into());
        }
        if result.is_ok()
            && self.driver.state() == DriverState::Powered
            && let Err(e) = self.write_stream_off()
        {
            result = Err(Error::Bus {
                during: "writing stream_off",
                source: e,
            });
        }
        if self.driver.state() != DriverState::Off {
            let down = if result.is_ok() {
                self.driver.power_down()
            } else {
                self.driver.force_power_down()
            };
            if let Err(e) = down {
                // Still off as far as the pins go: `force_power_down` tried every step.
                let _ = self.driver.force_power_down();
                if result.is_ok() {
                    result = Err(e.into());
                }
            }
        }
        result
    }

    /// Gives the camera a lens.
    pub fn set_lens(&mut self, lens: Option<LensControl>) {
        self.lens = lens;
    }

    /// The lens, if the camera has one.
    pub fn lens(&self) -> Option<&LensControl> {
        self.lens.as_ref()
    }

    /// The lens, mutably.
    pub fn lens_mut(&mut self) -> Option<&mut LensControl> {
        self.lens.as_mut()
    }

    /// Moves the lens to `position` for frame `frame` on (see [`LensControl::request_at`]):
    /// written at the start of `frame - delay`, or at once when that has passed (or before
    /// streaming).
    pub fn request_lens_at(&mut self, frame: u64, position: i32) -> Result<()> {
        let current = self.current_frame().filter(|_| self.is_streaming());
        let now = self.now();
        match &mut self.lens {
            Some(l) => l.request_at(frame, position, current, now),
            None => Err(Error::InvalidConfig("the camera has no lens".into())),
        }
    }

    /// Moves the lens to `position` as soon as possible.
    pub fn set_lens_position(&mut self, position: i32) -> Result<()> {
        let now = self.now();
        match &mut self.lens {
            Some(l) => l.request_at(0, position, None, now),
            None => Err(Error::InvalidConfig("the camera has no lens".into())),
        }
    }

    /// Phase detection data in this layout (`imx708`) is decoded from the embedded data.
    pub fn set_pdaf_format(&mut self, format: Option<String>) {
        self.pdaf.format = format;
    }

    /// The phase detection data decoded so far.
    pub fn pdaf(&self) -> &PdafFrames {
        &self.pdaf
    }

    fn lens_frame_start(&mut self, seq: u64, at: Option<Duration>) -> Result<()> {
        if self.lens.is_none() {
            return Ok(());
        }
        let at = at.unwrap_or_else(|| self.now());
        match &mut self.lens {
            Some(l) => {
                if seq == 0 {
                    l.restart();
                }
                l.frame_start(seq, at)
            }
            None => Ok(()),
        }
    }

    /// Fills in where the lens was for the frame `f` describes.
    fn with_lens(&self, seq: u64, mut f: FrameControls) -> FrameControls {
        if let (Some(l), Some(t)) = (&self.lens, self.timing()) {
            let line = f64::from(t.width + t.hblank) / t.pixel_rate.max(1) as f64;
            let readout = Duration::from_secs_f64(line * f64::from(t.height));
            f.lens = l.frame(seq, f.exposure, readout);
        }
        f
    }

    fn decode_pdaf(&mut self, seq: u64, data: &[u8]) {
        if self.pdaf.format.is_none() {
            return;
        }
        if let Some(m) = self.driver.mode() {
            let bits = u32::from(m.code.bit_depth().unwrap_or(10));
            let width = m.timing.width;
            self.pdaf.decode(seq, data, width, bits);
        }
    }
}

#[cfg(test)]
#[path = "sensor_tests.rs"]
mod tests;
