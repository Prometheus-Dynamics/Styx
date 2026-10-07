//! Sensor registers over a plain bus: Lemnos's register maps (`lemnos_hal::register`:
//! [`I2cRegisters`] on any embedded-hal `I2c`, [`SpiRegisters`] on any `SpiDevice`, blocking
//! or async) run the driver directly. Lemnos owns the encoding (8/16-bit register addresses,
//! big-endian values, bursts of consecutive registers, one transfer per write message); this
//! module only makes them a [`DriverBus`] / [`AsyncDriverBus`] (no V4L2 controls) and builds
//! them from a description.

#![allow(deprecated)]

use embedded_hal::i2c::I2c;
use embedded_hal::spi::SpiDevice;
use lemnos_hal::register::RegisterResult;

pub use lemnos_hal::register::{AddressWidth, Endian, I2cRegisters, SpiRegisters};

use crate::bus::{AsyncDriverBus, AsyncRegisterBus, BusResult, DriverBus, RegisterBus};
use crate::bus_error::{BusError, BusErrorKind};
use crate::desc::{RegWrite, SensorDescription};

/// Longest burst the driver's sensors get with `burst_writes` (data bytes per transfer):
/// what Styx has run on the CM5 (Lemnos allows up to `lemnos_hal::register::MAX_BURST`).
pub const MAX_BURST: usize = 32;

impl<T: I2c<Error: 'static>> DriverBus for I2cRegisters<T> {}
impl<T: embedded_hal_async::i2c::I2c<Error: 'static>> AsyncDriverBus for I2cRegisters<T> {}
impl<T: SpiDevice<Error: 'static>> DriverBus for SpiRegisters<T> {}
impl<T: embedded_hal_async::spi::SpiDevice<Error: 'static>> AsyncDriverBus for SpiRegisters<T> {}

/// A Lemnos register map as the driver's bus.
#[deprecated(
    since = "2.0.0",
    note = "`RegisterBus` is `lemnos_hal::RegisterBus` now: implement `DriverBus` for the map \
            (`impl DriverBus for MyMap {}`) and pass it to the driver directly"
)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registers<R>(pub R);

impl<R: RegisterBus> RegisterBus for Registers<R> {
    type BusError = R::BusError;
    fn endian(&self) -> Endian {
        self.0.endian()
    }
    fn read_burst(&mut self, address: u16, buf: &mut [u8]) -> RegisterResult<(), R::BusError> {
        self.0.read_burst(address, buf)
    }
    fn write_burst(&mut self, address: u16, data: &[u8]) -> RegisterResult<(), R::BusError> {
        self.0.write_burst(address, data)
    }
    fn read(&mut self, address: u16, bytes: u8) -> RegisterResult<u32, R::BusError> {
        self.0.read(address, bytes)
    }
    fn write(&mut self, address: u16, bytes: u8, value: u32) -> RegisterResult<(), R::BusError> {
        self.0.write(address, bytes, value)
    }
    fn write_sequence(&mut self, writes: &[RegWrite]) -> RegisterResult<(), R::BusError> {
        self.0.write_sequence(writes)
    }
}

impl<R: AsyncRegisterBus> AsyncRegisterBus for Registers<R> {
    type BusError = R::BusError;
    fn endian(&self) -> Endian {
        self.0.endian()
    }
    async fn read_burst(
        &mut self,
        address: u16,
        buf: &mut [u8],
    ) -> RegisterResult<(), R::BusError> {
        self.0.read_burst(address, buf).await
    }
    async fn write_burst(&mut self, address: u16, data: &[u8]) -> RegisterResult<(), R::BusError> {
        self.0.write_burst(address, data).await
    }
    async fn read(&mut self, address: u16, bytes: u8) -> RegisterResult<u32, R::BusError> {
        self.0.read(address, bytes).await
    }
    async fn write(
        &mut self,
        address: u16,
        bytes: u8,
        value: u32,
    ) -> RegisterResult<(), R::BusError> {
        self.0.write(address, bytes, value).await
    }
    async fn write_sequence(&mut self, writes: &[RegWrite]) -> RegisterResult<(), R::BusError> {
        self.0.write_sequence(writes).await
    }
}

impl<R: RegisterBus<BusError: 'static>> DriverBus for Registers<R> {}
impl<R: AsyncRegisterBus<BusError: 'static>> AsyncDriverBus for Registers<R> {}

impl SensorDescription {
    /// The sensor's registers on `i2c`: its 7-bit `i2c_address`, its register address width,
    /// and bursts of up to [`MAX_BURST`] bytes when it takes `burst_writes` (one transfer per
    /// register write otherwise).
    pub fn i2c_registers<I>(&self, i2c: I) -> BusResult<I2cRegisters<I>> {
        let s = &self.sensor;
        let address = s
            .i2c_address
            .and_then(|a| u8::try_from(a).ok())
            .filter(|a| *a <= 0x7f)
            .ok_or_else(|| {
                BusError::new(
                    BusErrorKind::InvalidInput,
                    "the description has no 7-bit i2c_address",
                )
            })?;
        let width = AddressWidth::from_bits(s.address_bits).ok_or_else(|| {
            BusError::new(
                BusErrorKind::InvalidInput,
                "register addresses of 8 or 16 bits",
            )
        })?;
        let burst = if s.burst_writes { MAX_BURST } else { 1 };
        Ok(I2cRegisters::new(i2c, address, width).with_bursts(burst))
    }
}

/// Reads `bytes` registers one at a time (separate transfers) and joins them big-endian: a
/// cross-check for a register map's burst read.
pub fn read_bytewise<B>(bus: &mut B, address: u16, bytes: u8) -> BusResult<u32>
where
    B: RegisterBus<BusError: 'static> + ?Sized,
{
    let mut value = 0u32;
    for i in 0..bytes {
        let byte = bus
            .read(address.wrapping_add(u16::from(i)), 1)
            .map_err(BusError::from_register)?;
        value = (value << 8) | byte;
    }
    Ok(value)
}
