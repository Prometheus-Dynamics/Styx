//! The register access a sensor driver runs over, and mock implementations for tests.
//!
//! [`RegisterBus`] / [`AsyncRegisterBus`] are what a sensor driver runs over: the sensor's
//! registers, or a kernel driver's V4L2 controls. Over a plain bus they are Lemnos's register
//! maps ([`I2cRegisters`](crate::I2cRegisters) / [`SpiRegisters`](crate::SpiRegisters) on any
//! embedded-hal `I2c` / `SpiDevice`, or any `lemnos_hal::RegisterBus` through
//! [`Registers`](crate::Registers)), which own the register encoding. Pins and power
//! sequencing are `styx_hal::SensorPins`. This crate never touches the kernel.

use alloc::borrow::ToOwned;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use embedded_hal::delay::DelayNs;
use styx_hal::{Blocking, SensorPins};

use crate::bus_error::{BusError, BusErrorKind};

use crate::desc::RegWrite;
use crate::fallback::KernelControl;

/// The result of a bus or pin operation.
pub type BusResult<T> = core::result::Result<T, BusError>;

/// Register access to one sensor, as the driver uses it: Lemnos's register map
/// (`lemnos_hal::RegisterBus`) plus a kernel driver's V4L2 controls, errors as [`BusError`].
/// Addresses are 8 or 16 bits as the description says; values wider than one byte are
/// consecutive registers, most significant byte first.
pub trait RegisterBus {
    /// Read `bytes` (1 to 4) bytes starting at `address`.
    fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32>;

    /// Write `bytes` (1 to 4) bytes starting at `address`.
    fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()>;

    /// Write several registers in order. Implementations may batch them into one transfer.
    fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
        for w in writes {
            self.write(w.address, w.bytes, w.value)?;
        }
        Ok(())
    }

    /// Sets V4L2 controls of a sensor a kernel driver owns ([`Backend::Kernel`]), in order and
    /// in one call (`VIDIOC_S_EXT_CTRLS`). Register buses do not have them
    /// ([`BusErrorKind::Unsupported`]).
    ///
    /// [`Backend::Kernel`]: crate::Backend::Kernel
    fn set_controls(&mut self, controls: &[(KernelControl, i64)]) -> BusResult<()> {
        let _ = controls;
        Err(BusError::new(
            BusErrorKind::Unsupported,
            "a register bus has no V4L2 controls",
        ))
    }
}

impl<B: RegisterBus + ?Sized> RegisterBus for &mut B {
    fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
        (**self).read(address, bytes)
    }
    fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
        (**self).write(address, bytes, value)
    }
    fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
        (**self).write_sequence(writes)
    }
    fn set_controls(&mut self, controls: &[(KernelControl, i64)]) -> BusResult<()> {
        (**self).set_controls(controls)
    }
}

/// [`RegisterBus`] over an async bus (an embedded-hal-async `I2c` through [`I2cRegisters`]).
/// A blocking bus is one through [`Blocking`] (its futures complete on the first poll).
///
/// [`I2cRegisters`]: crate::I2cRegisters
#[allow(async_fn_in_trait)]
pub trait AsyncRegisterBus {
    /// See [`RegisterBus::read`].
    async fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32>;

    /// See [`RegisterBus::write`].
    async fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()>;

    /// See [`RegisterBus::write_sequence`].
    async fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
        for w in writes {
            self.write(w.address, w.bytes, w.value).await?;
        }
        Ok(())
    }

    /// See [`RegisterBus::set_controls`].
    async fn set_controls(&mut self, controls: &[(KernelControl, i64)]) -> BusResult<()> {
        let _ = controls;
        Err(BusError::new(
            BusErrorKind::Unsupported,
            "a register bus has no V4L2 controls",
        ))
    }
}

impl<B: AsyncRegisterBus + ?Sized> AsyncRegisterBus for &mut B {
    async fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
        (**self).read(address, bytes).await
    }
    async fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
        (**self).write(address, bytes, value).await
    }
    async fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
        (**self).write_sequence(writes).await
    }
    async fn set_controls(&mut self, controls: &[(KernelControl, i64)]) -> BusResult<()> {
        (**self).set_controls(controls).await
    }
}

impl<B: RegisterBus> AsyncRegisterBus for Blocking<B> {
    async fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
        self.0.read(address, bytes)
    }
    async fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
        self.0.write(address, bytes, value)
    }
    async fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
        self.0.write_sequence(writes)
    }
    async fn set_controls(&mut self, controls: &[(KernelControl, i64)]) -> BusResult<()> {
        self.0.set_controls(controls)
    }
}

/// Pins for boards where power, clock and reset are handled by firmware or the kernel: every
/// role reports [`BusErrorKind::NotFound`] (optional steps are skipped, required ones fail) and
/// delays sleep. Needs `std` (for the sleep); without it, use `styx_hal::BoardPins` or
/// implement [`SensorPins`] with the platform's delay.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPins;

fn not_found(what: &str, role: &str) -> BusError {
    BusError::new(
        BusErrorKind::NotFound,
        format!("no {what} with role '{role}'"),
    )
}

#[cfg(feature = "std")]
impl DelayNs for NoPins {
    fn delay_ns(&mut self, ns: u32) {
        std::thread::sleep(Duration::from_nanos(u64::from(ns)));
    }
}

#[cfg(feature = "std")]
impl SensorPins for NoPins {
    type Error = BusError;
    fn set_gpio(&mut self, role: &str, _: bool) -> BusResult<()> {
        Err(not_found("gpio", role))
    }
    fn set_clock(&mut self, role: &str, _: Option<u32>) -> BusResult<()> {
        Err(not_found("clock", role))
    }
    fn set_supply(&mut self, role: &str, _: bool) -> BusResult<()> {
        Err(not_found("supply", role))
    }
}

/// A recorded bus operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusOp {
    /// A read and the value returned.
    Read {
        /// Address.
        address: u16,
        /// Width.
        bytes: u8,
        /// Value returned.
        value: u32,
    },
    /// A write.
    Write(RegWrite),
}

/// A register bus backed by a byte map, recording every operation.
#[derive(Debug, Clone, Default)]
pub struct MockBus {
    /// Register contents by address (unset registers read as 0).
    pub registers: BTreeMap<u16, u8>,
    /// Every operation, in order.
    pub log: Vec<BusOp>,
    /// Writes to these addresses fail with an I/O error.
    pub fail_writes: BTreeSet<u16>,
    /// Every [`RegisterBus::set_controls`] call (kernel-driven sensors), in order.
    pub control_log: Vec<Vec<(KernelControl, i64)>>,
}

impl MockBus {
    /// An empty bus.
    pub fn new() -> Self {
        Self::default()
    }

    /// Preset a register value of `bytes` bytes (e.g. the chip id).
    pub fn with_register(mut self, address: u16, bytes: u8, value: u32) -> Self {
        self.store(address, bytes, value);
        self
    }

    fn store(&mut self, address: u16, bytes: u8, value: u32) {
        for i in 0..bytes {
            let shift = 8 * u32::from(bytes - 1 - i);
            self.registers
                .insert(address.wrapping_add(u16::from(i)), (value >> shift) as u8);
        }
    }

    /// The value of `bytes` bytes at `address`.
    pub fn value(&self, address: u16, bytes: u8) -> u32 {
        (0..bytes).fold(0u32, |acc, i| {
            (acc << 8)
                | u32::from(
                    *self
                        .registers
                        .get(&address.wrapping_add(u16::from(i)))
                        .unwrap_or(&0),
                )
        })
    }

    /// The writes recorded so far.
    pub fn writes(&self) -> Vec<RegWrite> {
        self.log
            .iter()
            .filter_map(|op| {
                if let BusOp::Write(w) = op {
                    Some(*w)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Forget recorded operations (register contents stay).
    pub fn clear_log(&mut self) {
        self.log.clear();
        self.control_log.clear();
    }

    /// The last value set for a V4L2 control.
    pub fn control(&self, control: KernelControl) -> Option<i64> {
        self.control_log
            .iter()
            .flatten()
            .rev()
            .find(|(c, _)| *c == control)
            .map(|(_, v)| *v)
    }
}

impl RegisterBus for MockBus {
    fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
        let value = self.value(address, bytes);
        self.log.push(BusOp::Read {
            address,
            bytes,
            value,
        });
        Ok(value)
    }

    fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
        if self.fail_writes.contains(&address) {
            return Err(BusError::other("injected write failure"));
        }
        self.store(address, bytes, value);
        self.log.push(BusOp::Write(RegWrite {
            address,
            value,
            bytes,
        }));
        Ok(())
    }

    fn set_controls(&mut self, controls: &[(KernelControl, i64)]) -> BusResult<()> {
        self.control_log.push(controls.to_vec());
        Ok(())
    }
}

/// A recorded pin operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinOp {
    /// GPIO set.
    Gpio(String, bool),
    /// Clock enabled at a rate or disabled.
    Clock(String, Option<u32>),
    /// Supply switched.
    Supply(String, bool),
    /// Delay requested.
    Delay(Duration),
}

/// Pins that record operations and do not sleep. Only roles added with
/// [`MockPins::with_roles`] exist; others report `NotFound`.
#[derive(Debug, Clone, Default)]
pub struct MockPins {
    /// Roles the board has.
    pub roles: BTreeSet<String>,
    /// Every operation, in order.
    pub log: Vec<PinOp>,
}

impl MockPins {
    /// Pins with the given roles.
    pub fn with_roles(roles: &[&str]) -> Self {
        Self {
            roles: roles.iter().map(|r| (*r).to_owned()).collect(),
            log: Vec::new(),
        }
    }

    fn check(&self, what: &str, role: &str) -> BusResult<()> {
        if self.roles.contains(role) {
            Ok(())
        } else {
            Err(not_found(what, role))
        }
    }
}

impl DelayNs for MockPins {
    fn delay_ns(&mut self, ns: u32) {
        self.log
            .push(PinOp::Delay(Duration::from_nanos(u64::from(ns))));
    }
    fn delay_us(&mut self, us: u32) {
        self.log
            .push(PinOp::Delay(Duration::from_micros(u64::from(us))));
    }
    fn delay_ms(&mut self, ms: u32) {
        self.log
            .push(PinOp::Delay(Duration::from_millis(u64::from(ms))));
    }
}

impl SensorPins for MockPins {
    type Error = BusError;
    fn set_gpio(&mut self, role: &str, value: bool) -> BusResult<()> {
        self.check("gpio", role)?;
        self.log.push(PinOp::Gpio(role.into(), value));
        Ok(())
    }
    fn set_clock(&mut self, role: &str, rate_hz: Option<u32>) -> BusResult<()> {
        self.check("clock", role)?;
        self.log.push(PinOp::Clock(role.into(), rate_hz));
        Ok(())
    }
    fn set_supply(&mut self, role: &str, on: bool) -> BusResult<()> {
        self.check("supply", role)?;
        self.log.push(PinOp::Supply(role.into(), on));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_bus_is_big_endian_and_logs() {
        let mut bus = MockBus::new().with_register(0x300a, 2, 0x9782);
        assert_eq!(bus.read(0x300a, 1).unwrap(), 0x97);
        assert_eq!(bus.read(0x300b, 1).unwrap(), 0x82);
        bus.write(0x3500, 3, 0x002820).unwrap();
        assert_eq!(bus.registers[&0x3501], 0x28);
        assert_eq!(bus.value(0x3500, 3), 0x2820);
        bus.write_sequence(&[RegWrite::byte(0x0100, 1)]).unwrap();
        assert_eq!(bus.writes().len(), 2);
        bus.fail_writes.insert(0x0100);
        assert!(bus.write(0x0100, 1, 0).is_err());
    }

    #[test]
    fn mock_pins_report_missing_roles() {
        let mut p = MockPins::with_roles(&["avdd"]);
        assert!(p.set_supply("avdd", true).is_ok());
        assert_eq!(
            p.set_gpio("reset", true).unwrap_err().kind(),
            BusErrorKind::NotFound
        );
        assert_eq!(
            NoPins.set_clock("x", None).unwrap_err().kind(),
            BusErrorKind::NotFound
        );
    }
}
