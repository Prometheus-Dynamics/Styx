//! The sensor side of a camera on Linux: the portable runtime's [`SensorState`] (bring-up,
//! frame starts, typed control requests, the values that produced each frame) on
//! `CLOCK_MONOTONIC`, plus what is Linux's own: serving the Styx sensor bridge's start and stop
//! requests, and errors as [`NativeError`]s.
//!
//! The sensor state is generic over the register bus and pins, so it runs over `styx-sensor`'s
//! [`MockBus`](styx_sensor::MockBus) in tests exactly as over I²C on the device.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use styx_kernel::bus::{StreamAction, StreamRequest};
use styx_runtime::{Controls, SensorState, StartFormat};
use styx_sensor::{
    Control, ControlRequest, Landing, RegisterBus, SensorDriver, SensorPins, Timing,
};

use crate::error::Result;

pub use styx_runtime::{
    BlankingHook, BringUpTimes, DEFAULT_WRITE_MARGIN, FrameControls, standby_problems,
};

/// The sensor driver plus the per-stream bookkeeping around it (the runtime's
/// [`SensorState`]). Build it with [`sensor_control`] (it then runs on `CLOCK_MONOTONIC`).
pub type SensorControl<B, P> = SensorState<B, P>;

/// What a start request must carry for the configuration userspace set up.
pub type ExpectedStart = StartFormat;

/// A sensor control on `CLOCK_MONOTONIC` (the clock of V4L2 buffer and event timestamps).
pub fn sensor_control<B: RegisterBus, P: SensorPins>(
    driver: SensorDriver<B, P>,
) -> SensorControl<B, P> {
    SensorState::new(driver).with_clock(styx_kernel::monotonic_now)
}

/// The format a bridge start request carries.
pub fn start_format(req: &StreamRequest) -> StartFormat {
    StartFormat {
        code: req.code,
        width: req.width,
        height: req.height,
        link_freq: req.link_freq,
    }
}

/// Serving the Styx sensor bridge's start and stop requests.
pub trait BridgeServe {
    /// Serves one bridge request: starts or stops the sensor. The result is the
    /// acknowledgement (an errno on failure).
    fn serve(&mut self, req: &StreamRequest) -> std::result::Result<(), i32> {
        self.serve_detailed(req).map_err(|(errno, _)| errno)
    }

    /// [`Self::serve`], with why it failed.
    fn serve_detailed(&mut self, req: &StreamRequest) -> std::result::Result<(), (i32, String)>;
}

impl<B: RegisterBus, P: SensorPins> BridgeServe for SensorState<B, P> {
    fn serve_detailed(&mut self, req: &StreamRequest) -> std::result::Result<(), (i32, String)> {
        let served = match req.action {
            StreamAction::Start => self.serve_start(Some(&start_format(req))),
            StreamAction::Stop => self.serve_stop(),
        };
        served.map_err(|e| {
            let errno = match e.kind {
                styx_runtime::styx_hal::ErrorKind::InvalidConfig => libc::EINVAL,
                _ => libc::EIO,
            };
            (errno, e.why)
        })
    }
}

/// Locks a shared control, recovering from a poisoned lock (the state is plain data).
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A cloneable handle for typed, frame-accurate controls of an open camera (the runtime's
/// [`Controls`], with [`NativeError`](crate::NativeError)s).
pub struct ControlHandle<B, P> {
    inner: Controls<B, P>,
}

impl<B, P> Clone for ControlHandle<B, P> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
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
        Self {
            inner: Controls::new(inner, blanking),
        }
    }

    /// The shared control.
    pub fn shared(&self) -> &Arc<Mutex<SensorControl<B, P>>> {
        self.inner.shared()
    }

    /// The runtime's handle.
    pub fn controls(&self) -> &Controls<B, P> {
        &self.inner
    }

    /// Asks for exposure, gain and/or frame duration as soon as possible; returns where each
    /// value lands (frame sequence numbers of this stream).
    pub fn request(&self, req: &ControlRequest) -> Result<Vec<Landing>> {
        Ok(self.inner.request(req)?)
    }

    /// Asks for values from frame `frame` on (e.g. the landing frame an algorithm computed from
    /// the control delays); returns where each value lands (later than `frame` when the request
    /// came too late for it).
    pub fn request_at(&self, frame: u64, req: &ControlRequest) -> Result<Vec<Landing>> {
        Ok(self.inner.request_at(frame, req)?)
    }

    /// [`Self::request_at`], but values due in the current frame are written at once when
    /// enough of the frame is left (see [`SensorState::request_at_now`]): a request made right
    /// after a frame's statistics arrive can land `delay` frames after that frame.
    pub fn request_at_now(&self, frame: u64, req: &ControlRequest) -> Result<Vec<Landing>> {
        Ok(self.inner.request_at_now(frame, req)?)
    }

    /// See [`SensorState::set_write_margin`].
    pub fn set_write_margin(&self, margin: Option<Duration>) {
        self.inner.set_write_margin(margin);
    }

    /// See [`SensorState::frame_time_left`] (now).
    pub fn frame_time_left(&self) -> Option<Duration> {
        self.inner.frame_time_left()
    }

    /// Sets the exposure time.
    pub fn set_exposure(&self, exposure: Duration) -> Result<Vec<Landing>> {
        Ok(self.inner.set_exposure(exposure)?)
    }

    /// Sets the total gain (analogue first, then digital).
    pub fn set_gain(&self, gain: f64) -> Result<Vec<Landing>> {
        Ok(self.inner.set_gain(gain)?)
    }

    /// Sets the frame duration (the frame length closest to it).
    pub fn set_frame_duration(&self, duration: Duration) -> Result<Vec<Landing>> {
        Ok(self.inner.set_frame_duration(duration)?)
    }

    /// Sets the frame rate (the frame length closest to it).
    pub fn set_frame_rate(&self, fps: f64) -> Result<Vec<Landing>> {
        Ok(self.inner.set_frame_rate(fps)?)
    }

    /// The values predicted for the latest started frame.
    pub fn current(&self) -> Option<FrameControls> {
        self.inner.current()
    }

    /// The values that produced frame `seq`.
    pub fn applied(&self, seq: u64) -> Option<FrameControls> {
        self.inner.applied(seq)
    }

    /// Timing of the active mode.
    pub fn timing(&self) -> Option<Timing> {
        self.inner.timing()
    }

    /// Frame rates the active mode allows, `(min, max)`.
    pub fn fps_range(&self) -> Option<(f64, f64)> {
        self.inner.fps_range()
    }

    /// Exposure limits at the latest frame's frame length, `(min, max)`.
    pub fn exposure_range(&self) -> Option<(Duration, Duration)> {
        self.inner.exposure_range()
    }

    /// Gain range of the sensor (analogue × digital), `(min, max)`.
    pub fn gain_range(&self) -> (f64, f64) {
        self.inner.gain_range()
    }

    /// Delay of each control in frames, as the description gives them.
    pub fn delay(&self, control: Control) -> u32 {
        self.inner.delay(control)
    }

    /// Whether the camera has a focus lens.
    pub fn has_lens(&self) -> bool {
        self.inner.has_lens()
    }

    /// The lens's driver position range and its delay in frames.
    pub fn lens_range(&self) -> Option<([i32; 2], u32)> {
        self.inner.lens_range()
    }

    /// Moves the lens to `position` (driver units) for frame `frame` on: written at the
    /// start of `frame - delay`, or at once when that has passed (or before streaming).
    pub fn request_lens_at(&self, frame: u64, position: i32) -> Result<()> {
        Ok(self.inner.request_lens_at(frame, position)?)
    }

    /// Moves the lens to `position` as soon as possible.
    pub fn set_lens_position(&self, position: i32) -> Result<()> {
        Ok(self.inner.set_lens_position(position)?)
    }

    /// Frame `seq`'s phase detection cells, when the sensor sends them.
    pub fn pdaf(&self, seq: u64) -> Option<Arc<[styx_sensor::lens::imx708_pdaf::Cell]>> {
        self.inner.pdaf(seq)
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
