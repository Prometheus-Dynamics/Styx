//! Sensor registers over a plain embedded-hal bus: [`I2cRegisters`] on any `I2c` and
//! [`SpiRegisters`] on any `SpiDevice`, blocking or async. The encoding (register address
//! width, big-endian values, bursts of consecutive registers) is written once and shared by
//! both.

use embedded_hal::i2c::I2c;
use embedded_hal::spi::{Operation, SpiDevice};

use crate::bus::{AsyncRegisterBus, BusResult, RegisterBus};
use crate::bus_error::{BusError, BusErrorKind};
use crate::desc::RegWrite;

/// Longest burst [`I2cRegisters::with_bursts`] sends in one transfer, in data bytes.
pub const MAX_BURST: usize = 32;

/// One transfer: up to two address bytes and [`MAX_BURST`] data bytes.
type Frame = [u8; 2 + MAX_BURST];

fn check_width(bytes: u8) -> BusResult<usize> {
    if (1..=4).contains(&bytes) {
        Ok(usize::from(bytes))
    } else {
        Err(BusError::new(
            BusErrorKind::InvalidInput,
            "register access of 1 to 4 bytes",
        ))
    }
}

fn address_bytes(address_bits: u8) -> BusResult<usize> {
    match address_bits {
        8 => Ok(1),
        16 => Ok(2),
        _ => Err(BusError::new(
            BusErrorKind::InvalidInput,
            "register addresses of 8 or 16 bits",
        )),
    }
}

/// Writes the register address (big-endian, `width` bytes) into `out`; returns its length.
fn encode_address(address: u16, width: usize, out: &mut [u8]) -> BusResult<usize> {
    if width == 1 {
        out[0] = u8::try_from(address).map_err(|_| {
            BusError::new(
                BusErrorKind::InvalidInput,
                "register address does not fit 8 bits",
            )
        })?;
    } else {
        out[..2].copy_from_slice(&address.to_be_bytes());
    }
    Ok(width)
}

/// Writes one register write (address, then the value's low `bytes` bytes, big-endian) into
/// `out`; returns its length.
fn encode_write(w: &RegWrite, width: usize, out: &mut Frame) -> BusResult<usize> {
    let n = check_width(w.bytes)?;
    if n < 4 && w.value >> (8 * n) != 0 {
        return Err(BusError::new(
            BusErrorKind::InvalidInput,
            "register value does not fit its bytes",
        ));
    }
    let a = encode_address(w.address, width, out)?;
    out[a..a + n].copy_from_slice(&w.value.to_be_bytes()[4 - n..]);
    Ok(a + n)
}

/// The next transfer of a sequence from `writes[i]`: the write and, when `burst` allows, the
/// writes to the following consecutive addresses that follow it in the sequence. Returns the
/// transfer's length in `out` and the index after it.
fn next_transfer(
    writes: &[RegWrite],
    mut i: usize,
    width: usize,
    burst: usize,
    out: &mut Frame,
) -> BusResult<(usize, usize)> {
    let w = writes[i];
    let mut len = encode_write(&w, width, out)?;
    let mut next = w.address.checked_add(u16::from(w.bytes));
    i += 1;
    while let Some(n) = writes.get(i) {
        let data = len - width + usize::from(n.bytes);
        if next != Some(n.address) || data > burst {
            break;
        }
        let mut one = [0u8; 2 + MAX_BURST];
        let l = encode_write(n, width, &mut one)?;
        out[len..len + l - width].copy_from_slice(&one[width..l]);
        len += l - width;
        next = n.address.checked_add(u16::from(n.bytes));
        i += 1;
    }
    Ok((len, i))
}

fn decode(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0, |v, b| (v << 8) | u32::from(*b))
}

/// A sensor's registers over an embedded-hal I²C bus (blocking `I2c` for [`RegisterBus`],
/// embedded-hal-async `I2c` for [`AsyncRegisterBus`]): register addresses of the description's
/// width, multi-byte values as consecutive registers most significant byte first (one
/// transfer), reads as one write-then-read transaction.
///
/// Every write is its own transfer (start, address, register, data, stop), as kernel drivers
/// do. Not several write messages in one combined transfer: on the CM5 (RP1 DesignWare I²C)
/// with the OV9782 that lost or misplaced writes (the SCCB target took a message's register
/// address as data when the repeated start was not issued). With [`Self::with_bursts`],
/// consecutive registers that follow each other in a sequence share one transfer (a single
/// message: the target auto-increments the register address).
#[derive(Debug)]
pub struct I2cRegisters<I> {
    i2c: I,
    address: u8,
    width: usize,
    burst: usize,
}

impl<I> I2cRegisters<I> {
    /// The sensor at 7-bit `address` on `i2c`, with `address_bits` (8 or 16) register
    /// addresses.
    pub fn new(i2c: I, address: u8, address_bits: u8) -> BusResult<Self> {
        if address > 0x7f {
            return Err(BusError::new(
                BusErrorKind::InvalidInput,
                "7-bit I2C address out of range",
            ));
        }
        Ok(Self {
            i2c,
            address,
            width: address_bytes(address_bits)?,
            burst: 1,
        })
    }

    /// Writes runs of consecutive registers in a sequence as one auto-incrementing transfer
    /// (address, then up to [`MAX_BURST`] data bytes), for sensors that take them (the
    /// description's `burst_writes`).
    pub fn with_bursts(mut self, on: bool) -> Self {
        self.burst = if on { MAX_BURST } else { 1 };
        self
    }

    /// The 7-bit address.
    pub fn address(&self) -> u8 {
        self.address
    }

    /// The bus.
    pub fn inner(&self) -> &I {
        &self.i2c
    }

    /// The bus, mutably.
    pub fn inner_mut(&mut self) -> &mut I {
        &mut self.i2c
    }

    /// The bus back.
    pub fn into_inner(self) -> I {
        self.i2c
    }
}

impl<I: I2c> I2cRegisters<I> {
    /// Reads `bytes` registers one at a time (separate transfers) and joins them big-endian:
    /// a cross-check for [`RegisterBus::read`]'s burst read.
    pub fn read_bytewise(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
        let mut value = 0u32;
        for i in 0..bytes {
            let v = RegisterBus::read(self, address.wrapping_add(u16::from(i)), 1)?;
            value = (value << 8) | v;
        }
        Ok(value)
    }
}

impl<I: I2c> RegisterBus for I2cRegisters<I> {
    fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
        let n = check_width(bytes)?;
        let mut reg = [0u8; 2];
        let a = encode_address(address, self.width, &mut reg)?;
        let mut buf = [0u8; 4];
        self.i2c
            .write_read(self.address, &reg[..a], &mut buf[..n])
            .map_err(|e| BusError::from_i2c(&e))?;
        Ok(decode(&buf[..n]))
    }

    fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
        self.write_sequence(&[RegWrite {
            address,
            value,
            bytes,
        }])
    }

    fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
        let mut i = 0;
        let mut frame = [0u8; 2 + MAX_BURST];
        while i < writes.len() {
            let (len, next) = next_transfer(writes, i, self.width, self.burst, &mut frame)?;
            self.i2c
                .write(self.address, &frame[..len])
                .map_err(|e| BusError::from_i2c(&e))?;
            i = next;
        }
        Ok(())
    }
}

impl<I: embedded_hal_async::i2c::I2c> AsyncRegisterBus for I2cRegisters<I> {
    async fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
        let n = check_width(bytes)?;
        let mut reg = [0u8; 2];
        let a = encode_address(address, self.width, &mut reg)?;
        let mut buf = [0u8; 4];
        self.i2c
            .write_read(self.address, &reg[..a], &mut buf[..n])
            .await
            .map_err(|e| BusError::from_i2c(&e))?;
        Ok(decode(&buf[..n]))
    }

    async fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
        self.write_sequence(&[RegWrite {
            address,
            value,
            bytes,
        }])
        .await
    }

    async fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
        let mut i = 0;
        let mut frame = [0u8; 2 + MAX_BURST];
        while i < writes.len() {
            let (len, next) = next_transfer(writes, i, self.width, self.burst, &mut frame)?;
            self.i2c
                .write(self.address, &frame[..len])
                .await
                .map_err(|e| BusError::from_i2c(&e))?;
            i = next;
        }
        Ok(())
    }
}

/// A sensor's registers over an embedded-hal SPI device (blocking or async): the register
/// address (8 or 16 bits, big-endian) with `read_flag` or-ed into its first byte for reads,
/// then the data, in one chip-select transaction; consecutive registers of a sequence share a
/// transaction with [`Self::with_bursts`]. Sensors with other SPI framings implement
/// [`RegisterBus`] themselves.
#[derive(Debug)]
pub struct SpiRegisters<S> {
    spi: S,
    width: usize,
    read_flag: u8,
    burst: usize,
}

impl<S> SpiRegisters<S> {
    /// Registers on `spi` with `address_bits` (8 or 16) addresses; reads set `read_flag` in the
    /// first address byte (commonly `0x80`).
    pub fn new(spi: S, address_bits: u8, read_flag: u8) -> BusResult<Self> {
        Ok(Self {
            spi,
            width: address_bytes(address_bits)?,
            read_flag,
            burst: 1,
        })
    }

    /// See [`I2cRegisters::with_bursts`].
    pub fn with_bursts(mut self, on: bool) -> Self {
        self.burst = if on { MAX_BURST } else { 1 };
        self
    }

    /// The device back.
    pub fn into_inner(self) -> S {
        self.spi
    }

    fn read_address(&self, address: u16) -> BusResult<([u8; 2], usize)> {
        let mut reg = [0u8; 2];
        let a = encode_address(address, self.width, &mut reg)?;
        reg[0] |= self.read_flag;
        Ok((reg, a))
    }
}

impl<S: SpiDevice> RegisterBus for SpiRegisters<S> {
    fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
        let n = check_width(bytes)?;
        let (reg, a) = self.read_address(address)?;
        let mut buf = [0u8; 4];
        self.spi
            .transaction(&mut [Operation::Write(&reg[..a]), Operation::Read(&mut buf[..n])])
            .map_err(|e| BusError::from_spi(&e))?;
        Ok(decode(&buf[..n]))
    }

    fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
        self.write_sequence(&[RegWrite {
            address,
            value,
            bytes,
        }])
    }

    fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
        let mut i = 0;
        let mut frame = [0u8; 2 + MAX_BURST];
        while i < writes.len() {
            let (len, next) = next_transfer(writes, i, self.width, self.burst, &mut frame)?;
            self.spi
                .write(&frame[..len])
                .map_err(|e| BusError::from_spi(&e))?;
            i = next;
        }
        Ok(())
    }
}

impl<S: embedded_hal_async::spi::SpiDevice> AsyncRegisterBus for SpiRegisters<S> {
    async fn read(&mut self, address: u16, bytes: u8) -> BusResult<u32> {
        let n = check_width(bytes)?;
        let (reg, a) = self.read_address(address)?;
        let mut buf = [0u8; 4];
        self.spi
            .transaction(&mut [Operation::Write(&reg[..a]), Operation::Read(&mut buf[..n])])
            .await
            .map_err(|e| BusError::from_spi(&e))?;
        Ok(decode(&buf[..n]))
    }

    async fn write(&mut self, address: u16, bytes: u8, value: u32) -> BusResult<()> {
        self.write_sequence(&[RegWrite {
            address,
            value,
            bytes,
        }])
        .await
    }

    async fn write_sequence(&mut self, writes: &[RegWrite]) -> BusResult<()> {
        let mut i = 0;
        let mut frame = [0u8; 2 + MAX_BURST];
        while i < writes.len() {
            let (len, next) = next_transfer(writes, i, self.width, self.burst, &mut frame)?;
            self.spi
                .write(&frame[..len])
                .await
                .map_err(|e| BusError::from_spi(&e))?;
            i = next;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(address: u16, value: u32, bytes: u8) -> RegWrite {
        RegWrite {
            address,
            value,
            bytes,
        }
    }

    #[test]
    fn encodes_writes_big_endian() {
        let mut f = [0u8; 2 + MAX_BURST];
        let n = encode_write(&w(0x0100, 1, 1), 2, &mut f).unwrap();
        assert_eq!(f[..n], [0x01, 0x00, 0x01]);
        let n = encode_write(&w(0x3500, 0x002820, 3), 2, &mut f).unwrap();
        assert_eq!(f[..n], [0x35, 0x00, 0x00, 0x28, 0x20]);
        let n = encode_write(&w(0x12, 0xab, 1), 1, &mut f).unwrap();
        assert_eq!(f[..n], [0x12, 0xab]);
        assert!(encode_write(&w(0x0100, 0x100, 1), 2, &mut f).is_err());
        assert!(encode_write(&w(0x0100, 0, 0), 2, &mut f).is_err());
        assert!(encode_write(&w(0x0100, 0, 5), 2, &mut f).is_err());
        assert!(encode_write(&w(0x0100, 0, 1), 1, &mut f).is_err());
        assert!(address_bytes(12).is_err());
    }

    #[test]
    fn bursts_join_consecutive_registers_up_to_the_limit() {
        let writes = [
            w(0x3800, 1, 1),
            w(0x3801, 2, 1),
            w(0x3802, 0x0304, 2),
            w(0x3805, 5, 1),
        ];
        let mut f = [0u8; 2 + MAX_BURST];
        let (n, next) = next_transfer(&writes, 0, 2, MAX_BURST, &mut f).unwrap();
        assert_eq!((f[..n].to_vec(), next), (vec![0x38, 0, 1, 2, 3, 4], 3));
        let (n, next) = next_transfer(&writes, 0, 2, 1, &mut f).unwrap();
        assert_eq!((n, next), (3, 1));
        let run: Vec<RegWrite> = (0..70)
            .map(|i| w(0x5000 + i, u32::from(i as u8), 1))
            .collect();
        let mut i = 0;
        let mut lens = Vec::new();
        while i < run.len() {
            let (n, next) = next_transfer(&run, i, 2, MAX_BURST, &mut f).unwrap();
            lens.push(n - 2);
            i = next;
        }
        assert_eq!(lens, [32, 32, 6]);
    }
}
