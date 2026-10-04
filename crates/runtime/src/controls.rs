//! A cloneable handle for typed, frame-accurate controls of an open camera.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::time::Duration;

use styx_sensor::lens::imx708_pdaf;
use styx_sensor::{Control, ControlRequest, Landing, RegisterBus, SensorPins, Timing};

use crate::error::{Error, Result};
use crate::sensor::{FrameControls, SensorState};
use crate::sync::{Shared, lock};

/// Called when a frame duration change lands on the sensor's timing: `(hblank, vblank)`, e.g.
/// to update a Linux receiver's `VBLANK` control for other readers.
#[cfg(feature = "std")]
pub type BlankingHook = Arc<dyn Fn(u32, u32) + Send + Sync>;
/// Called when a frame duration change lands on the sensor's timing: `(hblank, vblank)`.
#[cfg(not(feature = "std"))]
pub type BlankingHook = alloc::rc::Rc<dyn Fn(u32, u32)>;

/// Typed, frame-accurate controls of a camera: exposure, gain and frame duration on the frame
/// they are asked for, the lens, and what produced each frame. Cloning shares the camera.
pub struct Controls<B, P> {
    inner: Shared<SensorState<B, P>>,
    blanking: Option<BlankingHook>,
}

impl<B, P> Clone for Controls<B, P> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            blanking: self.blanking.clone(),
        }
    }
}

impl<B, P> core::fmt::Debug for Controls<B, P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Controls").finish_non_exhaustive()
    }
}

impl<B: RegisterBus, P: SensorPins> Controls<B, P> {
    /// A handle on a shared sensor state.
    pub fn new(inner: Shared<SensorState<B, P>>, blanking: Option<BlankingHook>) -> Self {
        Self { inner, blanking }
    }

    /// The shared sensor state.
    pub fn shared(&self) -> &Shared<SensorState<B, P>> {
        &self.inner
    }

    fn blanking(&self, timing: Option<Timing>, req: &ControlRequest) {
        if let (Some(hook), Some(t), Some(d)) = (&self.blanking, timing, req.frame_duration) {
            let fl = t.frame_length_for_duration(d);
            hook(t.hblank, fl.vblank);
        }
    }

    /// Asks for exposure, gain and/or frame duration as soon as possible; returns where each
    /// value lands (frame sequence numbers of this stream).
    pub fn request(&self, req: &ControlRequest) -> Result<Vec<Landing>> {
        let (landings, timing) = {
            let mut c = lock(&self.inner);
            let l = c.request(req)?;
            (l, c.timing())
        };
        self.blanking(timing, req);
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
        self.blanking(timing, req);
        Ok(landings)
    }

    /// [`Self::request_at`], but values due in the current frame are written at once when
    /// enough of the frame is left (see [`SensorState::request_at_now`]): a request made right
    /// after a frame's statistics arrive can land `delay` frames after that frame.
    pub fn request_at_now(&self, frame: u64, req: &ControlRequest) -> Result<Vec<Landing>> {
        let (landings, timing) = {
            let mut c = lock(&self.inner);
            let first = c.next_frame();
            let now = c.now();
            let l = if c.clock().is_some() {
                c.request_at_now(frame.max(first), req, now)?
            } else {
                // No clock, no way to tell how much of the frame is left.
                c.request_at(frame.max(first), req)?
            };
            (l, c.timing())
        };
        self.blanking(timing, req);
        Ok(landings)
    }

    /// See [`SensorState::set_write_margin`].
    pub fn set_write_margin(&self, margin: Option<Duration>) {
        lock(&self.inner).set_write_margin(margin);
    }

    /// See [`SensorState::frame_time_left`] (now).
    pub fn frame_time_left(&self) -> Option<Duration> {
        let c = lock(&self.inner);
        c.clock()?;
        c.frame_time_left(c.now())
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
            return Err(Error::InvalidConfig(alloc::format!("frame rate {fps}")));
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

    /// Whether the camera has a focus lens.
    pub fn has_lens(&self) -> bool {
        lock(&self.inner).lens().is_some()
    }

    /// The lens's driver position range and its delay in frames.
    pub fn lens_range(&self) -> Option<([i32; 2], u32)> {
        lock(&self.inner).lens().map(|l| (l.range(), l.delay()))
    }

    /// Moves the lens to `position` (driver units) for frame `frame` on: written at the
    /// start of `frame - delay`, or at once when that has passed (or before streaming).
    pub fn request_lens_at(&self, frame: u64, position: i32) -> Result<()> {
        lock(&self.inner).request_lens_at(frame, position)
    }

    /// Moves the lens to `position` as soon as possible.
    pub fn set_lens_position(&self, position: i32) -> Result<()> {
        lock(&self.inner).set_lens_position(position)
    }

    /// Frame `seq`'s phase detection cells, when the sensor sends them.
    pub fn pdaf(&self, seq: u64) -> Option<Arc<[imx708_pdaf::Cell]>> {
        lock(&self.inner).pdaf().get(seq)
    }
}
