//! The two sensor back ends behind one driver: registers over I²C with the bridge's power
//! control (a sensor Styx drives itself), or a kernel driver's V4L2 controls on its subdevice
//! (a sensor with an upstream driver). [`styx_sensor::SensorDriver`] runs over either.

use std::io;
use std::io::{PipeReader, PipeWriter};
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;

use lemnos_linux::hal::{I2cBus, IoError};
use styx_kernel::bus::{SensorBridge, StreamRequest, StreamState};
use styx_kernel::subdev::Subdev;
use styx_kernel::v4l2::{ControlValue, ControlWhich, Controls};
use styx_sensor::lemnos_hal::register::{RegisterBus as RegisterMap, RegisterError};
use styx_sensor::styx_hal::embedded_hal::delay::DelayNs;
use styx_sensor::{BusError, BusResult, KernelControl, RegWrite, RegisterBus, SensorPins};

use crate::device::BridgeDevice;
use crate::regbus::{BridgePins, I2cRegisterBus};

/// A sensor's V4L2 controls on its kernel driver's subdevice, as a [`RegisterBus`] without
/// registers: [`RegisterBus::set_controls`] is `VIDIOC_S_EXT_CTRLS`.
#[derive(Debug, Clone)]
pub struct SubdevBus {
    subdev: Arc<Subdev>,
}

impl SubdevBus {
    /// Controls of `subdev`.
    pub fn new(subdev: Arc<Subdev>) -> Self {
        Self { subdev }
    }

    /// The subdevice.
    pub fn subdev(&self) -> &Arc<Subdev> {
        &self.subdev
    }
}

fn no_registers() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "a kernel driver owns the sensor's registers",
    )
}

impl RegisterBus for SubdevBus {
    fn read(&mut self, _: u16, _: u8) -> BusResult<u32> {
        Err(no_registers().into())
    }

    fn write(&mut self, _: u16, _: u8, _: u32) -> BusResult<()> {
        Err(no_registers().into())
    }

    fn set_controls(&mut self, controls: &[(KernelControl, i64)]) -> BusResult<()> {
        let values: Vec<(u32, ControlValue)> = controls
            .iter()
            .map(|(c, v)| {
                let v = i32::try_from(*v).unwrap_or(if *v < 0 { i32::MIN } else { i32::MAX });
                (c.cid(), ControlValue::Integer(v))
            })
            .collect();
        Ok(self
            .subdev
            .set_controls(ControlWhich::Current, &values)
            .map_err(io::Error::from)?)
    }
}

/// The register bus of a camera: I²C for a bridged sensor, the kernel driver's controls
/// otherwise.
#[derive(Debug)]
pub enum SensorBus {
    /// Registers over I²C.
    I2c(I2cRegisterBus<I2cBus>),
    /// A kernel driver's V4L2 controls.
    Kernel(SubdevBus),
}

/// A Lemnos register error on i2c-dev as a [`BusError`], keeping the errno (`EREMOTEIO`, a
/// NACK; `EBUSY`, a kernel driver owns the address; ...) and Lemnos's classification.
pub fn i2c_error(e: RegisterError<IoError>) -> BusError {
    BusError::from_register(e.map_bus(IoError::into_io))
}

impl RegisterBus for SensorBus {
    fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
        match self {
            SensorBus::I2c(b) => RegisterMap::read(b, address, bytes).map_err(i2c_error),
            SensorBus::Kernel(b) => b.read(address, bytes),
        }
    }

    fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
        match self {
            SensorBus::I2c(b) => RegisterMap::write(b, address, bytes, value).map_err(i2c_error),
            SensorBus::Kernel(b) => b.write(address, bytes, value),
        }
    }

    fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
        match self {
            SensorBus::I2c(b) => RegisterMap::write_sequence(b, writes).map_err(i2c_error),
            SensorBus::Kernel(b) => b.write_sequence(writes),
        }
    }

    fn set_controls(&mut self, controls: &[(KernelControl, i64)]) -> BusResult<()> {
        match self {
            SensorBus::I2c(b) => RegisterBus::set_controls(b, controls),
            SensorBus::Kernel(b) => b.set_controls(controls),
        }
    }
}

/// The pins of a camera: the bridge's power control, or none (a kernel driver powers its
/// sensor with runtime PM).
#[derive(Debug)]
pub enum CameraPins {
    /// The bridge switches supplies and clock.
    Bridge(BridgePins<Arc<SensorBridge>>),
    /// Nothing to switch: every role is absent (optional steps are skipped).
    None(styx_sensor::NoPins),
}

impl DelayNs for CameraPins {
    fn delay_ns(&mut self, ns: u32) {
        match self {
            CameraPins::Bridge(p) => p.delay_ns(ns),
            CameraPins::None(p) => p.delay_ns(ns),
        }
    }
}

impl SensorPins for CameraPins {
    type Error = BusError;

    fn set_gpio(&mut self, role: &str, value: bool) -> BusResult<()> {
        match self {
            CameraPins::Bridge(p) => p.set_gpio(role, value),
            CameraPins::None(p) => p.set_gpio(role, value),
        }
    }

    fn set_clock(&mut self, role: &str, rate_hz: Option<u32>) -> BusResult<()> {
        match self {
            CameraPins::Bridge(p) => p.set_clock(role, rate_hz),
            CameraPins::None(p) => p.set_clock(role, rate_hz),
        }
    }

    fn set_supply(&mut self, role: &str, on: bool) -> BusResult<()> {
        match self {
            CameraPins::Bridge(p) => p.set_supply(role, on),
            CameraPins::None(p) => p.set_supply(role, on),
        }
    }
}

/// The bridge's place for a sensor a kernel driver owns: the receiver's `STREAMON` starts the
/// sensor itself (`s_stream`), so there are no requests to serve; the session starts the
/// control schedule just before it ([`BridgeDevice::kernel_driven`]). Its descriptor (one end
/// of a pipe nobody writes) never becomes ready.
#[derive(Debug)]
pub(crate) struct KernelBridge {
    reader: PipeReader,
    _writer: PipeWriter,
}

impl KernelBridge {
    pub(crate) fn new() -> io::Result<Self> {
        let (reader, writer) = std::io::pipe()?;
        Ok(Self {
            reader,
            _writer: writer,
        })
    }
}

impl AsFd for KernelBridge {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.reader.as_fd()
    }
}

impl BridgeDevice for KernelBridge {
    fn try_next_request(&self) -> io::Result<Option<StreamRequest>> {
        Ok(None)
    }

    fn acknowledge(&self, _: &StreamRequest, _: Result<(), i32>) -> io::Result<()> {
        Ok(())
    }

    fn power(&self) -> io::Result<bool> {
        Ok(false)
    }

    fn set_power(&self, _: bool) -> io::Result<()> {
        Ok(())
    }

    fn stream_state(&self) -> io::Result<StreamState> {
        Ok(StreamState::Idle)
    }

    fn kernel_driven(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i2c_errors_keep_their_errno() {
        let e = i2c_error(RegisterError::bus(
            styx_sensor::BusErrorKind::Nack,
            IoError::from(io::Error::from_raw_os_error(121)),
        ));
        assert_eq!(e.kind(), styx_sensor::BusErrorKind::Nack);
        assert_eq!(e.code(), Some(121));
        assert_eq!(io::Error::from(e).raw_os_error(), Some(121));
    }

    #[test]
    fn the_kernel_bridge_never_asks_and_is_always_idle() {
        let b = KernelBridge::new().unwrap();
        assert!(b.try_next_request().unwrap().is_none());
        assert!(b.is_idle().unwrap() && !b.power().unwrap() && b.kernel_driven());
        b.set_power(true).unwrap();
        assert!(!b.power().unwrap());
    }
}
