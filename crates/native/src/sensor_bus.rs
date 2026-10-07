//! The two sensor back ends behind one driver: registers over I²C with the bridge's power
//! control (a sensor Styx drives itself), or a kernel driver's V4L2 controls on its subdevice
//! (a sensor with an upstream driver). [`styx_sensor::SensorDriver`] runs over either.
//!
//! The registers are Lemnos's register map ([`styx_sensor::I2cRegisters`], i.e.
//! `lemnos_hal::I2cRegisters`) on Lemnos's i2c-dev bus (`lemnos_linux::hal::I2cBus`, the
//! sensor's address claimed with `I2C_SLAVE`, never forced). What stays here is camera
//! specific: the bridge's power control as [`SensorPins`] and the kernel driver's controls.

use std::io;
use std::io::{PipeReader, PipeWriter};
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
use std::time::Duration;

use lemnos_linux::hal::{I2cBus, IoError, StdDelay};
use styx_kernel::bus::{SensorBridge, StreamRequest, StreamState};
use styx_kernel::subdev::Subdev;
use styx_kernel::v4l2::{ControlValue, ControlWhich, Controls};
use styx_sensor::lemnos_hal::register::{RegisterError, RegisterResult};
use styx_sensor::styx_hal::embedded_hal::delay::DelayNs;
use styx_sensor::{
    BusError, BusErrorKind, BusResult, DriverBus, I2cRegisters, KernelControl, RegWrite,
    RegisterBus, SensorPins,
};

use crate::device::BridgeDevice;

/// Something that switches the sensor's supplies and clock together: the bridge's
/// `STYX_CID_POWER`, or a recorder in tests.
pub trait PowerSwitch {
    /// Switch on or off.
    fn set_power(&self, on: bool) -> io::Result<()>;
}

impl PowerSwitch for SensorBridge {
    fn set_power(&self, on: bool) -> io::Result<()> {
        SensorBridge::set_power(self, on)
    }
}

impl<P: PowerSwitch + ?Sized> PowerSwitch for std::sync::Arc<P> {
    fn set_power(&self, on: bool) -> io::Result<()> {
        (**self).set_power(on)
    }
}

/// [`SensorPins`] for a sensor whose supplies and clock the bridge owns (the CM5 camera
/// regulator and the fixed 24 MHz clock) and that has no GPIO lines.
///
/// The bridge switches all of them at once, so the first supply or clock turned on powers the
/// sensor and the last one turned off powers it down; the roles only track the description's
/// sequence. GPIO roles report `NotFound` (optional steps are skipped).
#[derive(Debug)]
pub struct BridgePins<S> {
    switch: S,
    supplies: Vec<(String, bool)>,
    clocks: Vec<(String, u32, bool)>,
    powered: bool,
    settle: Duration,
}

impl<S: PowerSwitch> BridgePins<S> {
    /// Pins for the given supply roles and `(clock role, rate in Hz)`; `settle` is waited after
    /// switching power on, before the sequence goes on.
    pub fn new(switch: S, supplies: &[&str], clocks: &[(&str, u32)], settle: Duration) -> Self {
        Self {
            switch,
            supplies: supplies.iter().map(|s| ((*s).to_owned(), false)).collect(),
            clocks: clocks
                .iter()
                .map(|(c, rate)| ((*c).to_owned(), *rate, false))
                .collect(),
            powered: false,
            settle,
        }
    }

    /// Whether this has switched the bridge on.
    #[cfg(test)]
    pub fn powered(&self) -> bool {
        self.powered
    }

    fn any_on(&self) -> bool {
        self.supplies.iter().any(|s| s.1) || self.clocks.iter().any(|c| c.2)
    }

    fn update(&mut self) -> io::Result<()> {
        let want = self.any_on();
        if want != self.powered {
            self.switch.set_power(want)?;
            self.powered = want;
            if want {
                std::thread::sleep(self.settle);
            }
        }
        Ok(())
    }
}

fn not_found(what: &str, role: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("no {what} '{role}' behind the bridge"),
    )
}

impl<S> DelayNs for BridgePins<S> {
    fn delay_ns(&mut self, ns: u32) {
        StdDelay.delay_ns(ns);
    }
}

impl<S: PowerSwitch> SensorPins for BridgePins<S> {
    type Error = BusError;

    fn set_gpio(&mut self, role: &str, _: bool) -> BusResult<()> {
        Err(not_found("gpio", role).into())
    }

    fn set_clock(&mut self, role: &str, rate_hz: Option<u32>) -> BusResult<()> {
        let clock = self
            .clocks
            .iter_mut()
            .find(|c| c.0 == role)
            .ok_or_else(|| not_found("clock", role))?;
        if let Some(rate) = rate_hz
            && clock.1 != 0
            && rate != clock.1
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("clock '{role}' runs at {} Hz, not {rate} Hz", clock.1),
            )
            .into());
        }
        clock.2 = rate_hz.is_some();
        Ok(self.update()?)
    }

    fn set_supply(&mut self, role: &str, on: bool) -> BusResult<()> {
        let supply = self
            .supplies
            .iter_mut()
            .find(|s| s.0 == role)
            .ok_or_else(|| not_found("supply", role))?;
        supply.1 = on;
        Ok(self.update()?)
    }
}

/// A sensor's V4L2 controls on its kernel driver's subdevice, as a [`DriverBus`] without
/// registers: [`DriverBus::set_controls`] is `VIDIOC_S_EXT_CTRLS`.
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

fn no_registers() -> RegisterError<BusError> {
    RegisterError::bus(
        BusErrorKind::Unsupported,
        BusError::new(
            BusErrorKind::Unsupported,
            "a kernel driver owns the sensor's registers",
        ),
    )
}

impl RegisterBus for SubdevBus {
    type BusError = BusError;

    fn read_burst(&mut self, _: u16, _: &mut [u8]) -> RegisterResult<(), BusError> {
        Err(no_registers())
    }

    fn write_burst(&mut self, _: u16, _: &[u8]) -> RegisterResult<(), BusError> {
        Err(no_registers())
    }
}

impl DriverBus for SubdevBus {
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
    I2c(I2cRegisters<I2cBus>),
    /// A kernel driver's V4L2 controls.
    Kernel(SubdevBus),
}

/// A Lemnos register error on i2c-dev as a [`BusError`], keeping the errno (`EREMOTEIO`, a
/// NACK; `EBUSY`, a kernel driver owns the address; ...) and Lemnos's classification.
pub fn i2c_error(e: RegisterError<IoError>) -> BusError {
    BusError::from_register(e.map_bus(IoError::into_io))
}

/// The bus error of an i2c-dev register map as a [`BusError`] (errno kept).
fn on_i2c<T>(r: RegisterResult<T, IoError>) -> RegisterResult<T, BusError> {
    r.map_err(|e| e.map_bus(|io| BusError::from(io.into_io())))
}

impl RegisterBus for SensorBus {
    type BusError = BusError;

    fn read_burst(&mut self, address: u16, buf: &mut [u8]) -> RegisterResult<(), BusError> {
        match self {
            SensorBus::I2c(b) => on_i2c(b.read_burst(address, buf)),
            SensorBus::Kernel(b) => b.read_burst(address, buf),
        }
    }

    fn write_burst(&mut self, address: u16, data: &[u8]) -> RegisterResult<(), BusError> {
        match self {
            SensorBus::I2c(b) => on_i2c(b.write_burst(address, data)),
            SensorBus::Kernel(b) => b.write_burst(address, data),
        }
    }

    fn read(&mut self, address: u16, bytes: u8) -> RegisterResult<u32, BusError> {
        match self {
            SensorBus::I2c(b) => on_i2c(b.read(address, bytes)),
            SensorBus::Kernel(b) => b.read(address, bytes),
        }
    }

    fn write(&mut self, address: u16, bytes: u8, value: u32) -> RegisterResult<(), BusError> {
        match self {
            SensorBus::I2c(b) => on_i2c(b.write(address, bytes, value)),
            SensorBus::Kernel(b) => b.write(address, bytes, value),
        }
    }

    fn write_sequence(&mut self, writes: &[RegWrite]) -> RegisterResult<(), BusError> {
        match self {
            SensorBus::I2c(b) => on_i2c(b.write_sequence(writes)),
            SensorBus::Kernel(b) => b.write_sequence(writes),
        }
    }
}

impl DriverBus for SensorBus {
    fn set_controls(&mut self, controls: &[(KernelControl, i64)]) -> BusResult<()> {
        match self {
            SensorBus::I2c(b) => DriverBus::set_controls(b, controls),
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
    use std::cell::RefCell;

    use styx_sensor::styx_hal::mock::{I2cTransfer, MockI2c, MockOp};
    use styx_sensor::{AddressWidth, MockBus, SensorDescription, SensorDriver, read_bytewise};

    use super::*;

    fn ov9782_bus() -> (I2cRegisters<MockI2c>, MockI2c) {
        let i2c = MockI2c::new()
            .with_target(0x60, AddressWidth::Bits16)
            .with_registers(0x60, 0x300a, &[0x97, 0x82]);
        (
            I2cRegisters::new(i2c.clone(), 0x60, AddressWidth::Bits16),
            i2c,
        )
    }

    #[test]
    fn two_byte_chip_id_is_one_burst_with_a_16_bit_address() {
        let (mut bus, i2c) = ov9782_bus();
        assert_eq!(bus.read(0x300a, 2).unwrap(), 0x9782);
        assert_eq!(read_bytewise(&mut bus, 0x300a, 2).unwrap(), 0x9782);
        let read = |a: u16, n| I2cTransfer {
            address: 0x60,
            ops: vec![MockOp::Write(a.to_be_bytes().to_vec()), MockOp::Read(n)],
        };
        assert_eq!(
            i2c.transfers(),
            [read(0x300a, 2), read(0x300a, 1), read(0x300b, 1)]
        );
    }

    /// The driver run over this bus writes exactly what it writes over the mock bus.
    #[test]
    fn driver_writes_match_the_mock_bus() {
        let desc = Arc::new(
            SensorDescription::from_file(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../sensor/sensors/ov9782.toml"
            ))
            .unwrap(),
        );
        let switch = PowerLog::default();
        let pins = BridgePins::new(
            &switch,
            &["avdd", "dovdd", "dvdd"],
            &[("xvclk", 24_000_000)],
            Duration::ZERO,
        );
        let (bus, i2c) = ov9782_bus();
        let mut real = SensorDriver::new(Arc::clone(&desc), bus, pins);
        let mock = MockBus::new().with_register(0x300a, 2, 0x9782);
        let mut fake = SensorDriver::new(desc, mock, styx_sensor::MockPins::default());
        for d in [&mut real as &mut dyn Steps, &mut fake] {
            d.bring_up();
        }
        let written: Vec<Vec<u8>> = i2c
            .transfers()
            .into_iter()
            .flat_map(|t| t.ops)
            .filter_map(|m| match m {
                MockOp::Write(b) if b.len() > 2 => Some(b),
                _ => None,
            })
            .collect();
        let expected: Vec<Vec<u8>> = fake
            .bus()
            .writes()
            .iter()
            .map(|w| {
                let mut m = w.address.to_be_bytes().to_vec();
                m.extend_from_slice(&w.value.to_be_bytes()[4 - usize::from(w.bytes)..]);
                m
            })
            .collect();
        assert_eq!(written, expected);
        assert_eq!(*switch.log.borrow(), [true]);
    }

    trait Steps {
        fn bring_up(&mut self);
    }

    impl<B: DriverBus, P: SensorPins> Steps for SensorDriver<B, P> {
        fn bring_up(&mut self) {
            self.power_up().unwrap();
            assert_eq!(self.verify_chip_id().unwrap(), 0x9782);
            self.init().unwrap();
            self.set_mode("1280x800", "raw10").unwrap();
            self.start_streaming().unwrap();
            self.stop_streaming().unwrap();
        }
    }

    #[derive(Default)]
    struct PowerLog {
        log: RefCell<Vec<bool>>,
    }

    impl PowerSwitch for &PowerLog {
        fn set_power(&self, on: bool) -> io::Result<()> {
            self.log.borrow_mut().push(on);
            Ok(())
        }
    }

    #[test]
    fn pins_switch_the_bridge_once_per_edge() {
        let switch = PowerLog::default();
        let mut pins = BridgePins::new(
            &switch,
            &["avdd", "dovdd"],
            &[("xvclk", 24_000_000)],
            Duration::ZERO,
        );
        pins.set_supply("avdd", true).unwrap();
        pins.set_supply("dovdd", true).unwrap();
        pins.set_clock("xvclk", Some(24_000_000)).unwrap();
        assert!(pins.powered());
        assert!(pins.set_clock("xvclk", Some(19_200_000)).is_err());
        assert_eq!(
            pins.set_gpio("reset", true).unwrap_err().kind(),
            BusErrorKind::NotFound
        );
        assert_eq!(
            pins.set_supply("vana", true).unwrap_err().kind(),
            BusErrorKind::NotFound
        );
        pins.set_clock("xvclk", None).unwrap();
        pins.set_supply("dovdd", false).unwrap();
        assert!(pins.powered());
        pins.set_supply("avdd", false).unwrap();
        assert!(!pins.powered());
        assert_eq!(*switch.log.borrow(), [true, false]);
    }

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
