//! The userspace sensor driver: runs a description over a register bus and sensor pins.
//!
//! [`SensorDriver`] is blocking ([`RegisterBus`], [`SensorPins`]); [`AsyncSensorDriver`] is the
//! same driver over async buses ([`AsyncRegisterBus`], [`AsyncSensorPins`]: an embedded-hal-async
//! `I2c` through [`I2cRegisters`](crate::I2cRegisters)). The logic (bring-up sequences, control
//! writes, register reads, the schedule) is written once, as async code in `core.rs`; the
//! blocking driver runs it over [`Blocking`] adapters, whose futures are ready when first
//! polled, so each call completes in one poll without an executor.

mod asynchronous;
mod core;

use ::core::future::Future;
use ::core::pin::pin;
use ::core::task::{Context, Poll, Waker};
use ::core::time::Duration;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use styx_hal::{Blocking, SensorPins};

pub use self::asynchronous::AsyncSensorDriver;
use self::core::DriverCore;
#[allow(unused_imports)]
use crate::bus::AsyncRegisterBus;
use crate::bus::RegisterBus;
use crate::desc::SensorDescription;
use crate::error::Result;
use crate::mbus::{ColorFilter, MbusCode};
use crate::schedule::{Applied, ControlScheduler, ControlSet, IssueBatch, Landing, Mismatch};
use crate::timing::Timing;

/// Power and streaming state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverState {
    /// Not powered.
    Off,
    /// Powered, not streaming.
    Powered,
    /// Streaming.
    Streaming,
}

/// The applied mode.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveMode {
    /// Mode name.
    pub mode: String,
    /// Format name.
    pub format: String,
    /// Media bus code.
    pub code: MbusCode,
    /// Timing at the current horizontal blanking.
    pub timing: Timing,
}

/// Typed control values for a frame. Unset fields are left unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ControlRequest {
    /// Exposure time.
    pub exposure: Option<Duration>,
    /// Total gain (analogue first, then digital).
    pub gain: Option<f64>,
    /// Frame duration (sets the frame length).
    pub frame_duration: Option<Duration>,
}

/// The typed values that produced a frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AppliedControls {
    /// Frame sequence number.
    pub frame: u64,
    /// Exposure time.
    pub exposure: Duration,
    /// Exposure in lines.
    pub exposure_lines: f64,
    /// Analogue gain.
    pub analog_gain: f64,
    /// Digital gain (1 when the sensor has none).
    pub digital_gain: f64,
    /// Frame length in lines.
    pub frame_length: u32,
    /// Frame duration.
    pub frame_duration: Duration,
    /// The register codes, and which of them were read back.
    pub codes: Applied,
}

/// Runs the driver's async logic over blocking adapters: every await in it is on a
/// [`Blocking`] future, which completes when first polled, so one poll finishes it.
fn complete<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("a blocking bus or pin implementation returned Pending"),
    }
}

/// The accessors and the pure (schedule-only) calls, the same on both drivers.
macro_rules! common_methods {
    () => {
        /// The description.
        pub fn description(&self) -> &SensorDescription {
            &self.core.desc
        }

        /// Whether a kernel driver owns the sensor ([`Backend::Kernel`]): controls are V4L2
        /// controls ([`RegisterBus::set_controls`]) and there are no registers.
        ///
        /// [`Backend::Kernel`]: crate::Backend::Kernel
        pub fn is_kernel(&self) -> bool {
            self.core.is_kernel()
        }

        /// Replaces the description, e.g. with one rebuilt from a kernel driver's control
        /// ranges after a format change. Not while streaming; the mode must be set again (the
        /// control schedule ends). Flips stay as they are.
        pub fn set_description(&mut self, desc: Arc<SensorDescription>) -> Result<()> {
            self.core.set_description(desc)
        }

        /// Current state.
        pub fn state(&self) -> DriverState {
            self.core.state
        }

        /// The applied mode.
        pub fn mode(&self) -> Option<&ActiveMode> {
            self.core.mode.as_ref()
        }

        /// The control scheduler (after `set_mode`).
        pub fn scheduler(&self) -> Option<&ControlScheduler> {
            self.core.scheduler.as_ref()
        }

        /// Output colour filter order with the current flips.
        pub fn color_filter(&self) -> ColorFilter {
            self.core.color_filter()
        }

        /// Convert typed controls to codes for `frame`.
        pub fn codes_for(&self, frame: u64, req: &ControlRequest) -> Result<ControlSet> {
            self.core.codes_for(frame, req)
        }

        /// Ask for typed values from frame `frame`. Returns where each lands. Nothing is
        /// written now: the writes go out at the frame starts that make them land.
        pub fn request(&mut self, frame: u64, req: &ControlRequest) -> Result<Vec<Landing>> {
            let set = self.core.codes_for(frame, req)?;
            self.core.request_codes(frame, &set)
        }

        /// Ask for raw codes from frame `frame`.
        pub fn request_codes(&mut self, frame: u64, set: &ControlSet) -> Result<Vec<Landing>> {
            self.core.request_codes(frame, set)
        }

        /// Record codes read back for a frame (e.g. from embedded data).
        pub fn report(&mut self, frame: u64, codes: &ControlSet) -> Result<Vec<Mismatch>> {
            self.core.report(frame, codes)
        }

        /// The typed values that produced a frame.
        pub fn applied(&self, frame: u64) -> Option<AppliedControls> {
            self.core.applied(frame)
        }
    };
}
pub(crate) use common_methods;

/// A sensor driven over a blocking register bus and pins.
#[derive(Debug)]
pub struct SensorDriver<B, P> {
    core: DriverCore<Blocking<B>, Blocking<P>>,
}

impl<B, P> SensorDriver<B, P> {
    /// A driver for a described sensor. Nothing is written until [`Self::power_up`].
    pub fn new(desc: Arc<SensorDescription>, bus: B, pins: P) -> Self {
        Self {
            core: DriverCore::new(desc, Blocking(bus), Blocking(pins)),
        }
    }

    /// The register bus.
    pub fn bus(&self) -> &B {
        &self.core.bus.0
    }

    /// The register bus, mutably.
    pub fn bus_mut(&mut self) -> &mut B {
        &mut self.core.bus.0
    }

    /// The pins.
    pub fn pins(&self) -> &P {
        &self.core.pins.0
    }

    /// Take the bus and pins back.
    pub fn into_parts(self) -> (B, P) {
        (self.core.bus.0, self.core.pins.0)
    }

    common_methods!();
}

impl<B: RegisterBus, P: SensorPins> SensorDriver<B, P> {
    /// Run the power-up sequence.
    pub fn power_up(&mut self) -> Result<()> {
        complete(self.core.power_up())
    }

    /// Stop streaming if needed and run the power-down sequence.
    pub fn power_down(&mut self) -> Result<()> {
        complete(self.core.power_down())
    }

    /// Power down without talking to the sensor: runs only the pin steps (supplies, clocks,
    /// GPIOs) of the power-down sequence, skipping its register writes, whatever the state.
    /// For a sensor that stopped answering on its bus (a stream-off write would fail first).
    /// Every pin step is attempted; the first failure is returned.
    pub fn force_power_down(&mut self) -> Result<()> {
        complete(self.core.force_power_down())
    }

    /// Read the chip id and check it. Returns the value read.
    pub fn verify_chip_id(&mut self) -> Result<u32> {
        complete(self.core.verify_chip_id())
    }

    /// Write the common init registers.
    pub fn init(&mut self) -> Result<()> {
        complete(self.core.init())
    }

    /// Apply a mode and format: format registers, mode registers, line and frame length,
    /// default exposure and gain, and flips. Starts a new control schedule.
    pub fn set_mode(&mut self, mode: &str, format: &str) -> Result<&ActiveMode> {
        complete(self.core.set_mode(mode, format))?;
        Ok(self.core.mode.as_ref().expect("just set"))
    }

    /// Change horizontal blanking (not while streaming). Returns the new timing.
    pub fn set_hblank(&mut self, hblank: u32) -> Result<Timing> {
        complete(self.core.set_hblank(hblank))
    }

    /// Set horizontal mirror and vertical flip (not while streaming when the Bayer order
    /// changes).
    pub fn set_flips(&mut self, hflip: bool, vflip: bool) -> Result<()> {
        complete(self.core.set_flips(hflip, vflip))
    }

    /// Select a test pattern by name (`off` disables it).
    pub fn set_test_pattern(&mut self, name: &str) -> Result<()> {
        complete(self.core.set_test_pattern(name))
    }

    /// Write values due before streaming, then the stream-on sequence. Frame numbering starts
    /// at 0.
    pub fn start_streaming(&mut self) -> Result<()> {
        complete(self.core.start_streaming())
    }

    /// Run the stream-off sequence. Values still pending are written, and the schedule
    /// restarts at frame 0 for the next start.
    pub fn stop_streaming(&mut self) -> Result<()> {
        complete(self.core.stop_streaming())
    }

    /// [`Self::request`], but writes due in the current frame (the last one started) go out
    /// now instead of at the next frame start (see [`ControlScheduler::request_now`]): the
    /// caller must know the current frame has not ended yet. Before streaming, values for frame
    /// 0 are written at once rather than with the stream-on sequence.
    pub fn request_now(&mut self, frame: u64, req: &ControlRequest) -> Result<Vec<Landing>> {
        complete(self.core.request_now(frame, req))
    }

    /// Frame `seq` started: write what is due (inside group hold) and return it.
    pub fn frame_start(&mut self, seq: u64) -> Result<IssueBatch> {
        complete(self.core.frame_start(seq))
    }

    /// Write what is due now, without waiting for the next frame start. Until the first
    /// [`Self::frame_start`] after starting the stream, the scheduler still counts values as
    /// landing on frame 0; call `frame_start(0)` first when frame 0 may already be exposing.
    pub fn issue_now(&mut self) -> Result<IssueBatch> {
        complete(self.core.issue_now())
    }

    /// Read a register (diagnostics: register read-back after bring-up).
    pub fn read_register(&mut self, address: u16, bytes: u8) -> Result<u32> {
        complete(self.core.read(address, bytes))
    }
}
