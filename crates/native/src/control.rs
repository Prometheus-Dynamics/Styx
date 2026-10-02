//! The sensor side of a camera: the userspace driver, the bridge's start/stop requests, frame
//! starts, typed control requests, and the values that produced each frame.
//!
//! This part is generic over the register bus and pins, so it runs over `styx-sensor`'s
//! [`MockBus`](styx_sensor::MockBus) in tests exactly as over I²C on the device.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use styx_kernel::bus::{StreamAction, StreamRequest};
use styx_sensor::{
    AppliedControls, Control, ControlRequest, ControlSet, DriverState, Landing, Mismatch, RegWrite,
    RegisterBus, SensorDescription, SensorDriver, SensorPins, Step, Timing,
};

use crate::error::{NativeError, Result};

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
        }
    }
}

/// What a start request must carry for the configuration userspace set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpectedStart {
    /// Media bus code.
    pub code: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Link frequency in Hz.
    pub link_freq: i64,
}

impl ExpectedStart {
    /// Whether the request matches.
    pub fn check(&self, req: &StreamRequest) -> std::result::Result<(), String> {
        let got = (req.code, req.width, req.height, req.link_freq);
        let exp = (self.code, self.width, self.height, self.link_freq);
        if got == exp {
            Ok(())
        } else {
            Err(format!("start request carries {got:?}, configured {exp:?}"))
        }
    }
}

/// Problems that would let the sensor stream (drive its lanes out of LP-11) before the bridge's
/// start event: a write before `stream_on` of a `stream_on` register value, or a software
/// reset (`0x0103`).
pub fn standby_problems(desc: &SensorDescription, mode: &str, format: &str) -> Vec<String> {
    fn writes(steps: &[Step]) -> impl Iterator<Item = &RegWrite> {
        steps.iter().filter_map(Step::as_write)
    }
    let mut problems = Vec::new();
    let on: Vec<&RegWrite> = writes(&desc.sequences.stream_on).collect();
    if on.is_empty() {
        problems.push("stream_on has no register writes".to_owned());
    }
    let mut check = |what: &str, steps: &[Step]| {
        for w in writes(steps) {
            if on
                .iter()
                .any(|s| s.address == w.address && s.value == w.value)
            {
                problems.push(format!("{what} writes {w}, the stream-on value"));
            }
            if w.address == 0x0103 && w.value & 1 != 0 {
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

/// The sensor driver plus the per-stream bookkeeping around it.
#[derive(Debug)]
pub struct SensorControl<B, P> {
    driver: SensorDriver<B, P>,
    last_start: Option<u64>,
    expected: Option<ExpectedStart>,
    frame_starts: u64,
    starts_served: u64,
}

impl<B: RegisterBus, P: SensorPins> SensorControl<B, P> {
    /// Wraps a driver.
    pub fn new(driver: SensorDriver<B, P>) -> Self {
        Self {
            driver,
            last_start: None,
            expected: None,
            frame_starts: 0,
            starts_served: 0,
        }
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

    /// Powers the sensor, checks its chip id, writes init and the mode. The sensor stays in
    /// software standby. Powers up only if off; an active mode is replaced.
    pub fn bring_up(&mut self, mode: &str, format: &str) -> Result<()> {
        let problems = standby_problems(self.driver.description(), mode, format);
        if !problems.is_empty() {
            return Err(NativeError::InvalidConfig(format!(
                "the description would leave standby before the start event: {}",
                problems.join("; ")
            )));
        }
        match self.driver.state() {
            DriverState::Streaming => return Err(NativeError::State("sensor is streaming")),
            DriverState::Off => {
                self.driver.power_up()?;
                if let Err(e) = self
                    .driver
                    .verify_chip_id()
                    .and_then(|_| self.driver.init())
                {
                    let _ = self.driver.power_down();
                    return Err(e.into());
                }
            }
            DriverState::Powered => {}
        }
        self.driver.set_mode(mode, format)?;
        self.last_start = None;
        Ok(())
    }

    /// Sets what the next start request must carry.
    pub fn expect_start(&mut self, expected: ExpectedStart) {
        self.expected = Some(expected);
    }

    /// Serves one bridge request: starts or stops the sensor. The result is the
    /// acknowledgement (an errno on failure).
    pub fn serve(&mut self, req: &StreamRequest) -> std::result::Result<(), i32> {
        self.serve_detailed(req).map_err(|(errno, _)| errno)
    }

    /// [`Self::serve`], with why it failed.
    pub fn serve_detailed(
        &mut self,
        req: &StreamRequest,
    ) -> std::result::Result<(), (i32, String)> {
        match req.action {
            StreamAction::Start => {
                if let Some(exp) = self.expected
                    && let Err(why) = exp.check(req)
                {
                    return Err((libc::EINVAL, why));
                }
                self.last_start = None;
                if let Err(e) = self.driver.start_streaming() {
                    // Whatever part of stream-on went through is undone (best effort).
                    let _ = self.write_stream_off();
                    return Err((libc::EIO, format!("starting the sensor: {e}")));
                }
                self.starts_served += 1;
                Ok(())
            }
            StreamAction::Stop => {
                self.last_start = None;
                if self.driver.state() != DriverState::Streaming {
                    return Ok(());
                }
                self.driver
                    .stop_streaming()
                    .map_err(|e| (libc::EIO, format!("stopping the sensor: {e}")))
            }
        }
    }

    /// Start requests served successfully.
    pub fn starts_served(&self) -> u64 {
        self.starts_served
    }

    /// Frame `seq` started: writes what is due. Repeated or older sequences are ignored.
    /// Returns the controls written.
    pub fn frame_start(&mut self, seq: u64) -> Result<ControlSet> {
        if self.last_start.is_some_and(|l| seq <= l) {
            return Ok(ControlSet::new());
        }
        self.last_start = Some(seq);
        self.frame_starts += 1;
        if self.driver.state() != DriverState::Streaming {
            return Ok(ControlSet::new());
        }
        Ok(self.driver.frame_start(seq)?.controls)
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
        if let Some(d) = req.frame_duration
            && d.is_zero()
        {
            return Err(NativeError::InvalidConfig("zero frame duration".into()));
        }
        if let Some(g) = req.gain
            && !(g.is_finite() && g > 0.0)
        {
            return Err(NativeError::InvalidConfig(format!("gain {g}")));
        }
        Ok(self.driver.request(frame, req)?)
    }

    /// The values that produced frame `seq` (predicted, or read back where reported).
    pub fn applied(&self, seq: u64) -> Option<FrameControls> {
        self.driver.applied(seq).as_ref().map(FrameControls::from)
    }

    /// The values predicted for the latest started frame (or frame 0 before streaming).
    pub fn latest(&self) -> Option<FrameControls> {
        self.applied(self.last_start.unwrap_or(0))
    }

    /// Records the control values read back from a frame's embedded data. Returns the values
    /// that differ from the prediction.
    pub fn report_embedded(&mut self, seq: u64, data: &[u8]) -> Result<Vec<Mismatch>> {
        let codes = self.driver.description().decode_embedded(data);
        if codes.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.driver.report(seq, &codes)?)
    }

    /// Writes the stream-off registers whatever the driver's state (a start that failed half
    /// way, a sensor left streaming).
    fn write_stream_off(&mut self) -> std::io::Result<()> {
        let off: Vec<RegWrite> = self
            .driver
            .description()
            .sequences
            .stream_off
            .iter()
            .filter_map(Step::as_write)
            .copied()
            .collect();
        self.driver.bus_mut().write_sequence(&off)
    }

    /// Puts the sensor back in standby and powers it down (whatever state it is in). Best
    /// effort: when the sensor does not answer (the stream-off writes fail), it is powered down
    /// anyway and the first error is returned.
    pub fn shut_down(&mut self) -> Result<()> {
        self.last_start = None;
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
            result = Err(NativeError::kernel("writing stream_off", e));
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
}

/// Locks a shared control, recovering from a poisoned lock (the state is plain data).
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Called when a frame duration change lands on the sensor's timing: `(hblank, vblank)`, e.g.
/// to update the bridge's `VBLANK` control for other readers.
pub type BlankingHook = Arc<dyn Fn(u32, u32) + Send + Sync>;

/// A cloneable handle for typed, frame-accurate controls of an open camera.
pub struct ControlHandle<B, P> {
    inner: Arc<Mutex<SensorControl<B, P>>>,
    blanking: Option<BlankingHook>,
}

impl<B, P> Clone for ControlHandle<B, P> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            blanking: self.blanking.clone(),
        }
    }
}

impl<B, P> std::fmt::Debug for ControlHandle<B, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlHandle").finish_non_exhaustive()
    }
}

impl<B: RegisterBus, P: SensorPins> ControlHandle<B, P> {
    /// A handle on a shared control.
    pub fn new(inner: Arc<Mutex<SensorControl<B, P>>>, blanking: Option<BlankingHook>) -> Self {
        Self { inner, blanking }
    }

    /// The shared control.
    pub fn shared(&self) -> &Arc<Mutex<SensorControl<B, P>>> {
        &self.inner
    }

    /// Asks for exposure, gain and/or frame duration as soon as possible; returns where each
    /// value lands (frame sequence numbers of this stream).
    pub fn request(&self, req: &ControlRequest) -> Result<Vec<Landing>> {
        let (landings, timing) = {
            let mut c = lock(&self.inner);
            let l = c.request(req)?;
            (l, c.timing())
        };
        if let (Some(hook), Some(t), Some(d)) = (&self.blanking, timing, req.frame_duration) {
            let fl = t.frame_length_for_duration(d);
            hook(t.hblank, fl.vblank);
        }
        Ok(landings)
    }

    /// Asks for values from frame `frame` on (e.g. the landing frame an algorithm computed from
    /// the control delays); returns where each value lands (later than `frame` when the request
    /// came too late for it).
    pub fn request_at(&self, frame: u64, req: &ControlRequest) -> Result<Vec<Landing>> {
        let (landings, timing) = {
            let mut c = lock(&self.inner);
            let first = c.next_frame();
            let l = c.request_at(frame.max(first), req)?;
            (l, c.timing())
        };
        if let (Some(hook), Some(t), Some(d)) = (&self.blanking, timing, req.frame_duration) {
            let fl = t.frame_length_for_duration(d);
            hook(t.hblank, fl.vblank);
        }
        Ok(landings)
    }

    /// Sets the exposure time.
    pub fn set_exposure(&self, exposure: Duration) -> Result<Vec<Landing>> {
        self.request(&ControlRequest {
            exposure: Some(exposure),
            ..Default::default()
        })
    }

    /// Sets the total gain (analogue first, then digital).
    pub fn set_gain(&self, gain: f64) -> Result<Vec<Landing>> {
        self.request(&ControlRequest {
            gain: Some(gain),
            ..Default::default()
        })
    }

    /// Sets the frame duration (the frame length closest to it).
    pub fn set_frame_duration(&self, duration: Duration) -> Result<Vec<Landing>> {
        self.request(&ControlRequest {
            frame_duration: Some(duration),
            ..Default::default()
        })
    }

    /// Sets the frame rate (the frame length closest to it).
    pub fn set_frame_rate(&self, fps: f64) -> Result<Vec<Landing>> {
        if !(fps.is_finite() && fps > 0.0) {
            return Err(NativeError::InvalidConfig(format!("frame rate {fps}")));
        }
        self.set_frame_duration(Duration::from_secs_f64(1.0 / fps))
    }

    /// The values predicted for the latest started frame.
    pub fn current(&self) -> Option<FrameControls> {
        lock(&self.inner).latest()
    }

    /// The values that produced frame `seq`.
    pub fn applied(&self, seq: u64) -> Option<FrameControls> {
        lock(&self.inner).applied(seq)
    }

    /// Timing of the active mode.
    pub fn timing(&self) -> Option<Timing> {
        lock(&self.inner).timing()
    }

    /// Frame rates the active mode allows, `(min, max)`.
    pub fn fps_range(&self) -> Option<(f64, f64)> {
        self.timing().map(|t| t.fps_range())
    }

    /// Exposure limits at the latest frame's frame length, `(min, max)`.
    pub fn exposure_range(&self) -> Option<(Duration, Duration)> {
        let c = lock(&self.inner);
        let t = c.timing()?;
        let fl = c
            .latest()
            .map_or(t.frame_length_default(), |a| a.frame_length);
        let l = t.exposure_limits(fl);
        Some((l.min, l.max))
    }

    /// Gain range of the sensor (analogue × digital), `(min, max)`.
    pub fn gain_range(&self) -> (f64, f64) {
        let c = lock(&self.inner);
        let ctl = &c.driver().description().controls;
        let a = &ctl.analog_gain;
        let (mut lo, mut hi) = (a.gain_for_code(a.min_code), a.gain_for_code(a.max_code));
        if let Some(d) = &ctl.digital_gain {
            lo *= d.gain_for_code(d.min_code);
            hi *= d.gain_for_code(d.max_code);
        }
        (lo, hi)
    }

    /// Delay of each control in frames, as the description gives them.
    pub fn delay(&self, control: Control) -> u32 {
        lock(&self.inner)
            .driver()
            .scheduler()
            .map_or(0, |s| s.delay(control))
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
