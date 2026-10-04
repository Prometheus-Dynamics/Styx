//! The traits a sensor driver runs over, and mock implementations for tests.
//!
//! [`RegisterBus`] and [`SensorPins`] are implemented elsewhere (over i2c-dev and the GPIO
//! character device in `styx-kernel`). This crate never touches the kernel.

use alloc::borrow::ToOwned;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use crate::bus_error::{BusError, BusErrorKind};

use crate::desc::RegWrite;
use crate::fallback::KernelControl;

/// The result of a bus or pin operation.
pub type BusResult<T> = core::result::Result<T, BusError>;

/// Register access to one sensor. Addresses are 8 or 16 bits as the description says; values
/// wider than one byte are consecutive registers, most significant byte first.
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

/// GPIO lines, clocks and supplies of one sensor, by role name.
///
/// Implementations return [`BusErrorKind::NotFound`] for roles the board does not have, so
/// that optional steps can be skipped.
pub trait SensorPins {
    /// Set a GPIO line to a logical value (the implementation applies the line's polarity).
    fn set_gpio(&mut self, role: &str, value: bool) -> BusResult<()>;

    /// Enable a clock at `rate_hz`, or disable it with `None`.
    fn set_clock(&mut self, role: &str, rate_hz: Option<u32>) -> BusResult<()>;

    /// Enable or disable a supply.
    fn set_supply(&mut self, role: &str, on: bool) -> BusResult<()>;

    /// Wait (with `std`, by default, `std::thread::sleep`).
    #[cfg(feature = "std")]
    fn delay(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }

    /// Wait. Without `std` there is no default: the platform provides it.
    #[cfg(not(feature = "std"))]
    fn delay(&mut self, duration: Duration);
}

impl<P: SensorPins + ?Sized> SensorPins for &mut P {
    fn set_gpio(&mut self, role: &str, value: bool) -> BusResult<()> {
        (**self).set_gpio(role, value)
    }
    fn set_clock(&mut self, role: &str, rate_hz: Option<u32>) -> BusResult<()> {
        (**self).set_clock(role, rate_hz)
    }
    fn set_supply(&mut self, role: &str, on: bool) -> BusResult<()> {
        (**self).set_supply(role, on)
    }
    fn delay(&mut self, duration: Duration) {
        (**self).delay(duration)
    }
}

/// Pins for boards where power, clock and reset are handled by firmware or the kernel: every
/// role reports [`BusErrorKind::NotFound`] (optional steps are skipped, required ones fail) and
/// delays sleep. Needs `std` (for the sleep); without it, implement [`SensorPins`] with the
/// platform's delay.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPins;

fn not_found(what: &str, role: &str) -> BusError {
    BusError::new(
        BusErrorKind::NotFound,
        format!("no {what} with role '{role}'"),
    )
}

#[cfg(feature = "std")]
impl SensorPins for NoPins {
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

impl SensorPins for MockPins {
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
    fn delay(&mut self, duration: Duration) {
        self.log.push(PinOp::Delay(duration));
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
