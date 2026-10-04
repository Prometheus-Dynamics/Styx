//! `styx-sensor`'s register bus and `styx-hal`'s sensor pins over the kernel: registers over
//! i2c-dev ([`I2cRegisterBus`], the generic embedded-hal register layer on [`I2cDevice`]) and
//! [`SensorPins`] over the bridge's power control.

use std::io;
use std::time::Duration;

use styx_kernel::bus::SensorBridge;
use styx_kernel::bus::i2c::I2cDevice;
use styx_sensor::styx_hal::embedded_hal::delay::DelayNs;
use styx_sensor::{BusError, BusResult, SensorPins};

/// A sensor's registers over i2c-dev: [`styx_sensor::I2cRegisters`] on the kernel's
/// [`I2cDevice`] (an embedded-hal `I2c`). One transfer per register write, or bursts of
/// consecutive registers with `with_bursts` (the description's `burst_writes`).
pub type I2cRegisterBus<T = I2cDevice> = styx_sensor::I2cRegisters<T>;

/// Longest burst [`I2cRegisterBus::with_bursts`] sends in one transfer, in data bytes.
pub use styx_sensor::MAX_BURST;

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
        std::thread::sleep(Duration::from_nanos(u64::from(ns)));
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Arc;

    use styx_sensor::styx_hal::mock::{I2cMessage, MockI2c};
    use styx_sensor::{BusErrorKind, MockBus, RegisterBus, SensorDescription, SensorDriver};

    use super::*;

    fn ov9782_bus() -> (I2cRegisterBus<MockI2c>, MockI2c) {
        let i2c = MockI2c::new(0x60, 16).with_register(0x300a, 2, 0x9782);
        (I2cRegisterBus::new(i2c.clone(), 0x60, 16).unwrap(), i2c)
    }

    #[test]
    fn two_byte_chip_id_is_one_burst_with_a_16_bit_address() {
        let (mut bus, i2c) = ov9782_bus();
        assert_eq!(bus.read(0x300a, 2).unwrap(), 0x9782);
        assert_eq!(bus.read_bytewise(0x300a, 2).unwrap(), 0x9782);
        let read = |a: u16, n| {
            vec![
                I2cMessage::Write(a.to_be_bytes().to_vec()),
                I2cMessage::Read(n),
            ]
        };
        assert_eq!(
            i2c.transactions(),
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
            .transactions()
            .into_iter()
            .flatten()
            .filter_map(|m| match m {
                I2cMessage::Write(b) if b.len() > 2 => Some(b),
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

    impl<B: RegisterBus, P: SensorPins> Steps for SensorDriver<B, P> {
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
}
