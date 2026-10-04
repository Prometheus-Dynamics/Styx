//! `styx-sensor`'s bus traits on Linux: [`RegisterBus`](styx_sensor::RegisterBus) is Lemnos's
//! register map on Lemnos's i2c-dev bus ([`I2cRegisterBus`]), [`SensorPins`] the bridge's power
//! control.

use std::io;
use std::time::Duration;

use lemnos_linux::hal::{I2cBus, StdDelay};
use styx_kernel::bus::SensorBridge;
use styx_sensor::styx_hal::embedded_hal::delay::DelayNs;
use styx_sensor::{BusError, BusResult, SensorPins};

/// A sensor's registers over i2c-dev: Lemnos's register map (big-endian register addresses of
/// the description's width, multi-byte values as consecutive registers, one transfer per
/// register write: combined write transfers lost or misplaced writes on the CM5's RP1 I²C with
/// the OV9782) on Lemnos's [`I2cBus`] (the address claimed, never forced).
pub type I2cRegisterBus = styx_sensor::I2cRegisters<I2cBus>;

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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use styx_sensor::BusErrorKind;

    use super::*;

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
