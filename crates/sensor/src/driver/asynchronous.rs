//! The driver over async buses.

use crate::Arc;

use styx_hal::AsyncSensorPins;

use super::core::DriverCore;
use super::{ActiveMode, AppliedControls, ControlRequest, DriverState, common_methods};
use crate::bus::AsyncDriverBus;
#[allow(unused_imports)]
use crate::bus::DriverBus;
use crate::desc::SensorDescription;
use crate::error::Result;
use crate::mbus::ColorFilter;
use crate::schedule::{ControlScheduler, ControlSet, IssueBatch, Landings, Mismatches};
use crate::timing::Timing;

/// [`SensorDriver`](super::SensorDriver) over an async register bus and pins (an
/// embedded-hal-async `I2c` through [`I2cRegisters`](crate::I2cRegisters), Embassy pins and
/// timer). Same logic, same results; every call that talks to the sensor is `async`. A
/// blocking bus or pins can be mixed in through [`styx_hal::Blocking`].
#[derive(Debug)]
pub struct AsyncSensorDriver<B, P> {
    core: DriverCore<B, P>,
}

impl<B, P> AsyncSensorDriver<B, P> {
    /// A driver for a described sensor. Nothing is written until [`Self::power_up`].
    pub fn new(desc: Arc<SensorDescription>, bus: B, pins: P) -> Self {
        Self {
            core: DriverCore::new(desc, bus, pins),
        }
    }

    /// The register bus.
    pub fn bus(&self) -> &B {
        &self.core.bus
    }

    /// The register bus, mutably.
    pub fn bus_mut(&mut self) -> &mut B {
        &mut self.core.bus
    }

    /// The pins.
    pub fn pins(&self) -> &P {
        &self.core.pins
    }

    /// Take the bus and pins back.
    pub fn into_parts(self) -> (B, P) {
        (self.core.bus, self.core.pins)
    }

    common_methods!();
}

impl<B: AsyncDriverBus, P: AsyncSensorPins> AsyncSensorDriver<B, P> {
    /// See [`SensorDriver::power_up`](super::SensorDriver::power_up).
    pub async fn power_up(&mut self) -> Result<()> {
        self.core.power_up().await
    }

    /// See [`SensorDriver::power_down`](super::SensorDriver::power_down).
    pub async fn power_down(&mut self) -> Result<()> {
        self.core.power_down().await
    }

    /// See [`SensorDriver::force_power_down`](super::SensorDriver::force_power_down).
    pub async fn force_power_down(&mut self) -> Result<()> {
        self.core.force_power_down().await
    }

    /// See [`SensorDriver::verify_chip_id`](super::SensorDriver::verify_chip_id).
    pub async fn verify_chip_id(&mut self) -> Result<u32> {
        self.core.verify_chip_id().await
    }

    /// See [`SensorDriver::init`](super::SensorDriver::init).
    pub async fn init(&mut self) -> Result<()> {
        self.core.init().await
    }

    /// See [`SensorDriver::set_mode`](super::SensorDriver::set_mode).
    pub async fn set_mode(&mut self, mode: &str, format: &str) -> Result<&ActiveMode> {
        self.core.set_mode(mode, format).await?;
        Ok(self.core.mode.as_ref().expect("just set"))
    }

    /// See [`SensorDriver::set_hblank`](super::SensorDriver::set_hblank).
    pub async fn set_hblank(&mut self, hblank: u32) -> Result<Timing> {
        self.core.set_hblank(hblank).await
    }

    /// See [`SensorDriver::set_flips`](super::SensorDriver::set_flips).
    pub async fn set_flips(&mut self, hflip: bool, vflip: bool) -> Result<()> {
        self.core.set_flips(hflip, vflip).await
    }

    /// See [`SensorDriver::set_test_pattern`](super::SensorDriver::set_test_pattern).
    pub async fn set_test_pattern(&mut self, name: &str) -> Result<()> {
        self.core.set_test_pattern(name).await
    }

    /// See [`SensorDriver::start_streaming`](super::SensorDriver::start_streaming).
    pub async fn start_streaming(&mut self) -> Result<()> {
        self.core.start_streaming().await
    }

    /// See [`SensorDriver::stop_streaming`](super::SensorDriver::stop_streaming).
    pub async fn stop_streaming(&mut self) -> Result<()> {
        self.core.stop_streaming().await
    }

    /// See [`SensorDriver::request_now`](super::SensorDriver::request_now).
    pub async fn request_now(&mut self, frame: u64, req: &ControlRequest) -> Result<Landings> {
        self.core.request_now(frame, req).await
    }

    /// See [`SensorDriver::frame_start`](super::SensorDriver::frame_start).
    pub async fn frame_start(&mut self, seq: u64) -> Result<IssueBatch> {
        self.core.frame_start(seq).await
    }

    /// See [`SensorDriver::issue_now`](super::SensorDriver::issue_now).
    pub async fn issue_now(&mut self) -> Result<IssueBatch> {
        self.core.issue_now().await
    }

    /// See [`SensorDriver::read_register`](super::SensorDriver::read_register).
    pub async fn read_register(&mut self, address: u16, bytes: u8) -> Result<u32> {
        self.core.read(address, bytes).await
    }
}
