//! A mock platform: `styx-hal`'s mock receiver and an OV9782 register model that allocates
//! nothing (registers in a fixed array, no write log).

#![allow(dead_code)]

use std::sync::Arc;
use std::task::{Context, Waker};
use std::time::Duration;

use styx_hal::embedded_hal::delay::DelayNs;
use styx_hal::mock::MockReceiver;
use styx_runtime::sync::{Lock, new_lock};
use styx_runtime::{Platform, SensorState};
use styx_sensor::{BusResult, RegisterBus, SensorDescription, SensorDriver, SensorPins};

/// Registers in a fixed array.
pub struct Registers {
    pub values: Box<[u8; 65536]>,
    pub writes: u64,
}

impl Registers {
    pub fn ov9782() -> Self {
        let mut values = Box::new([0u8; 65536]);
        values[0x300a] = 0x97;
        values[0x300b] = 0x82;
        Self { values, writes: 0 }
    }

    pub fn value(&self, address: u16, bytes: u8) -> u32 {
        (0..bytes).fold(0, |v, i| {
            v << 8 | u32::from(self.values[usize::from(address.wrapping_add(u16::from(i)))])
        })
    }
}

impl RegisterBus for Registers {
    fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
        Ok(self.value(address, bytes))
    }

    fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
        for i in 0..bytes {
            let shift = 8 * u32::from(bytes - 1 - i);
            self.values[usize::from(address.wrapping_add(u16::from(i)))] = (value >> shift) as u8;
        }
        self.writes += 1;
        Ok(())
    }
}

/// Pins with no roles (optional power steps are skipped), no waiting.
pub struct Pins;

impl DelayNs for Pins {
    fn delay_ns(&mut self, _ns: u32) {}
}

impl SensorPins for Pins {
    type Error = styx_sensor::BusError;
    fn set_gpio(&mut self, role: &str, _value: bool) -> BusResult<()> {
        Err(not_found(role))
    }
    fn set_clock(&mut self, role: &str, _rate_hz: Option<u32>) -> BusResult<()> {
        Err(not_found(role))
    }
    fn set_supply(&mut self, role: &str, _on: bool) -> BusResult<()> {
        Err(not_found(role))
    }
}

fn not_found(role: &str) -> styx_sensor::BusError {
    styx_sensor::BusError::new(styx_sensor::BusErrorKind::NotFound, role.to_string())
}

pub type Sensor = Lock<SensorState<Registers, Pins>>;

/// The mock platform.
pub struct Mock;

impl Platform for Mock {
    type Receiver = MockReceiver;
    type Sensor = Sensor;
}

pub fn description() -> Arc<SensorDescription> {
    Arc::new(
        SensorDescription::from_file(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../sensor/sensors/ov9782.toml"
        ))
        .unwrap(),
    )
}

/// The OV9782 brought up in 1280x800 raw10, on a fake clock.
pub fn sensor() -> Arc<Sensor> {
    fn clock() -> Duration {
        Duration::from_secs(1)
    }
    let mut s = SensorState::new(SensorDriver::new(description(), Registers::ov9782(), Pins))
        .with_clock(clock);
    s.bring_up("1280x800", "raw10").unwrap();
    Arc::new(new_lock(s))
}

/// A context whose waker does nothing (a superloop).
pub fn noop() -> Context<'static> {
    Context::from_waker(Waker::noop())
}

pub fn config(buffers: u32) -> styx_hal::ReceiverConfig {
    styx_hal::ReceiverConfig {
        bus: styx_hal::Bus::Other,
        bus_code: 0x3007,
        fourcc: *b"pBAA",
        width: 64,
        height: 4,
        stride: Some(128),
        buffers,
        memory: styx_hal::BufferSource::Own,
        embedded: None,
        frame_starts: true,
    }
}
