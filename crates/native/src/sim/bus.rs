//! A simulated sensor's registers: a byte map the driver writes and the frame producer reads
//! (the flips), shared between them, without the operation log of `MockBus` (a long-running
//! camera writes exposure and gain every frame).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use styx_sensor::lemnos_hal::register::RegisterResult;
use styx_sensor::{BusError, BusResult, DriverBus, KernelControl, RegisterBus};

use crate::fake::lock;

/// The registers of a simulated sensor (clones share them).
#[derive(Clone, Debug, Default)]
pub struct SimBus {
    registers: Arc<Mutex<BTreeMap<u16, u8>>>,
}

impl SimBus {
    /// A sensor whose chip id reads `value` (`bytes` bytes at `address`).
    pub(crate) fn with_chip_id(address: u16, bytes: u8, value: u32) -> Self {
        let bus = Self::default();
        bus.store(address, bytes, value);
        bus
    }

    fn store(&self, address: u16, bytes: u8, value: u32) {
        let mut r = lock(&self.registers);
        for i in 0..bytes {
            let shift = 8 * u32::from(bytes - 1 - i);
            r.insert(address.wrapping_add(u16::from(i)), (value >> shift) as u8);
        }
    }

    /// The byte at `address` (0 when never written).
    pub(crate) fn byte(&self, address: u16) -> u8 {
        lock(&self.registers).get(&address).copied().unwrap_or(0)
    }

    fn value(&self, address: u16, bytes: u8) -> u32 {
        let r = lock(&self.registers);
        (0..bytes).fold(0, |acc, i| {
            (acc << 8)
                | u32::from(
                    r.get(&address.wrapping_add(u16::from(i)))
                        .copied()
                        .unwrap_or(0),
                )
        })
    }
}

impl RegisterBus for SimBus {
    type BusError = BusError;

    fn read_burst(&mut self, address: u16, buf: &mut [u8]) -> RegisterResult<(), BusError> {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = self.byte(address.wrapping_add(i as u16));
        }
        Ok(())
    }

    fn write_burst(&mut self, address: u16, data: &[u8]) -> RegisterResult<(), BusError> {
        let mut r = lock(&self.registers);
        for (i, b) in data.iter().enumerate() {
            r.insert(address.wrapping_add(i as u16), *b);
        }
        Ok(())
    }

    fn read(&mut self, address: u16, bytes: u8) -> RegisterResult<u32, BusError> {
        Ok(self.value(address, bytes))
    }

    fn write(&mut self, address: u16, bytes: u8, value: u32) -> RegisterResult<(), BusError> {
        self.store(address, bytes, value);
        Ok(())
    }
}

impl DriverBus for SimBus {
    fn set_controls(&mut self, _controls: &[(KernelControl, i64)]) -> BusResult<()> {
        Ok(())
    }
}

/// A simulated sensor's power switch: its bridge's.
#[derive(Clone)]
pub struct SimPower(pub(crate) Arc<crate::fake_bridge::FakeBridge>);

impl std::fmt::Debug for SimPower {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SimPower")
    }
}

impl crate::sensor_bus::PowerSwitch for SimPower {
    fn set_power(&self, on: bool) -> std::io::Result<()> {
        crate::sensor_bus::PowerSwitch::set_power(&*self.0, on)
    }
}
