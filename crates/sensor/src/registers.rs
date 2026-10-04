//! Sensor registers over a plain bus: Lemnos's register maps (`lemnos_hal::register`:
//! [`I2cRegisters`] on any embedded-hal `I2c`, [`SpiRegisters`] on any `SpiDevice`, blocking
//! or async) run the driver. Lemnos owns the encoding (8/16-bit register addresses, big-endian
//! values, bursts of consecutive registers, one transfer per write message); this module only
//! makes them the driver's [`RegisterBus`] / [`AsyncRegisterBus`] (errors as [`BusError`]),
//! and builds them from a description.

use embedded_hal::i2c::I2c;
use embedded_hal::spi::SpiDevice;
use lemnos_hal::register::asynch::RegisterBus as AsyncRegisterMap;
use lemnos_hal::register::{RegisterBus as RegisterMap, RegisterError};

pub use lemnos_hal::register::{AddressWidth, Endian, I2cRegisters, SpiRegisters};

use crate::bus::{AsyncRegisterBus, BusResult, RegisterBus};
use crate::bus_error::{BusError, BusErrorKind};
use crate::desc::{RegWrite, SensorDescription};

/// Longest burst the driver's sensors get with `burst_writes` (data bytes per transfer):
/// what Styx has run on the CM5 (Lemnos allows up to `lemnos_hal::register::MAX_BURST`).
pub const MAX_BURST: usize = 32;

fn bus<E: core::fmt::Debug + 'static>(e: RegisterError<E>) -> BusError {
    BusError::from_register(e)
}

/// Implements the driver's traits over a Lemnos register map type.
macro_rules! register_map {
    ($ty:ident, $blocking:path, $asynch:path) => {
        impl<T: $blocking> RegisterBus for $ty<T>
        where
            T::Error: 'static,
        {
            fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
                RegisterMap::read(self, address, bytes).map_err(bus)
            }
            fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
                RegisterMap::write(self, address, bytes, value).map_err(bus)
            }
            fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
                RegisterMap::write_sequence(self, writes).map_err(bus)
            }
        }

        impl<T: $asynch> AsyncRegisterBus for $ty<T>
        where
            T::Error: 'static,
        {
            async fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
                AsyncRegisterMap::read(self, address, bytes)
                    .await
                    .map_err(bus)
            }
            async fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
                AsyncRegisterMap::write(self, address, bytes, value)
                    .await
                    .map_err(bus)
            }
            async fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
                AsyncRegisterMap::write_sequence(self, writes)
                    .await
                    .map_err(bus)
            }
        }
    };
}

register_map!(I2cRegisters, I2c, embedded_hal_async::i2c::I2c);
register_map!(SpiRegisters, SpiDevice, embedded_hal_async::spi::SpiDevice);

/// Any other Lemnos register map (`lemnos_hal::RegisterBus`, blocking or async) as the
/// driver's bus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registers<R>(pub R);

impl<R: RegisterMap> RegisterBus for Registers<R>
where
    R::BusError: 'static,
{
    fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
        self.0.read(address, bytes).map_err(bus)
    }
    fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
        self.0.write(address, bytes, value).map_err(bus)
    }
    fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
        self.0.write_sequence(writes).map_err(bus)
    }
}

impl<R: AsyncRegisterMap> AsyncRegisterBus for Registers<R>
where
    R::BusError: 'static,
{
    async fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
        self.0.read(address, bytes).await.map_err(bus)
    }
    async fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
        self.0.write(address, bytes, value).await.map_err(bus)
    }
    async fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
        self.0.write_sequence(writes).await.map_err(bus)
    }
}

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
pub fn read_bytewise<B: RegisterBus + ?Sized>(
    bus: &mut B,
    address: u16,
    bytes: u8,
) -> BusResult<u32> {
    let mut value = 0u32;
    for i in 0..bytes {
        value = (value << 8) | bus.read(address.wrapping_add(u16::from(i)), 1)?;
    }
    Ok(value)
}
