//! embedded-hal 1.0 over the Linux devices: [`I2cDevice`] as an `I2c`, a requested GPIO line
//! as an `OutputPin`. Generic bus code (Styx's sensor register layer, any embedded-hal driver)
//! then runs on Linux unchanged. These are the pieces slated to move into Lemnos.

use std::borrow::Borrow;
use std::fmt;
use std::io;

use embedded_hal::digital;
use embedded_hal::i2c::{self, NoAcknowledgeSource, Operation};

use super::gpio::GpioLines;
use super::i2c::{I2C_M_RD, I2cDevice, I2cMsg, MAX_MESSAGES, msg_len};

/// An I/O error as an embedded-hal error. `Debug` prints like `Display` (the OS message and
/// errno), so a driver that only knows `Debug` still logs something readable.
pub struct IoError(pub io::Error);

impl fmt::Debug for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for IoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<io::Error> for IoError {
    fn from(e: io::Error) -> Self {
        IoError(e)
    }
}

impl From<IoError> for io::Error {
    fn from(e: IoError) -> Self {
        e.0
    }
}

impl i2c::Error for IoError {
    fn kind(&self) -> i2c::ErrorKind {
        match self.0.raw_os_error() {
            // i2c-dev / the adapters: no ACK on the address (ENXIO) or on data (EREMOTEIO).
            Some(libc::ENXIO) => i2c::ErrorKind::NoAcknowledge(NoAcknowledgeSource::Address),
            Some(libc::EREMOTEIO) => i2c::ErrorKind::NoAcknowledge(NoAcknowledgeSource::Unknown),
            Some(libc::EAGAIN) => i2c::ErrorKind::ArbitrationLoss,
            Some(libc::EIO) => i2c::ErrorKind::Bus,
            _ => i2c::ErrorKind::Other,
        }
    }
}

impl digital::Error for IoError {
    fn kind(&self) -> digital::ErrorKind {
        digital::ErrorKind::Other
    }
}

impl i2c::ErrorType for I2cDevice {
    type Error = IoError;
}

/// Messages per `I2C_RDWR` built on the stack (more take a heap array).
const STACK_MESSAGES: usize = 8;

impl i2c::I2c for I2cDevice {
    /// One `I2C_RDWR` call (repeated starts between the operations, one stop). Only the
    /// address claimed at open is allowed: `I2C_RDWR` would otherwise reach addresses a kernel
    /// driver owns, which the `I2C_SLAVE` claim exists to prevent.
    fn transaction(
        &mut self,
        address: u8,
        operations: &mut [Operation<'_>],
    ) -> Result<(), IoError> {
        if u16::from(address) != self.address() {
            return Err(IoError(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "I2C address {address:#04x}: this device claimed {:#04x}",
                    self.address()
                ),
            )));
        }
        if operations.is_empty() || operations.len() > MAX_MESSAGES {
            return Err(IoError(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} messages in one transfer (1..={MAX_MESSAGES})",
                    operations.len()
                ),
            )));
        }
        let msg = |op: &mut Operation<'_>| -> io::Result<I2cMsg> {
            Ok(match op {
                Operation::Write(data) => I2cMsg {
                    addr: u16::from(address),
                    flags: 0,
                    len: msg_len(data.len())?,
                    // The kernel only reads write buffers.
                    buf: data.as_ptr().cast_mut(),
                },
                Operation::Read(buf) => I2cMsg {
                    addr: u16::from(address),
                    flags: I2C_M_RD,
                    len: msg_len(buf.len())?,
                    buf: buf.as_mut_ptr(),
                },
            })
        };
        if operations.len() <= STACK_MESSAGES {
            let mut msgs = [I2cMsg {
                addr: 0,
                flags: 0,
                len: 0,
                buf: std::ptr::null_mut(),
            }; STACK_MESSAGES];
            for (m, op) in msgs.iter_mut().zip(operations.iter_mut()) {
                *m = msg(op)?;
            }
            Ok(self.rdwr(&mut msgs[..operations.len()])?)
        } else {
            let mut msgs = operations
                .iter_mut()
                .map(msg)
                .collect::<io::Result<Vec<_>>>()?;
            Ok(self.rdwr(&mut msgs)?)
        }
    }
}

/// One line of a [`GpioLines`] request as an embedded-hal `OutputPin` (logical levels: an
/// active-low request inverts). `L` is the request or a shared handle to it (`&GpioLines`,
/// `Arc<GpioLines>`), so several pins can share one request.
#[derive(Debug)]
pub struct GpioPin<L> {
    lines: L,
    offset: u32,
}

impl<L: Borrow<GpioLines>> GpioPin<L> {
    /// The line at chip offset `offset` of `lines`.
    pub fn new(lines: L, offset: u32) -> io::Result<Self> {
        if !lines.borrow().offsets().contains(&offset) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("line {offset} not in this request"),
            ));
        }
        Ok(Self { lines, offset })
    }
}

impl<L> digital::ErrorType for GpioPin<L> {
    type Error = IoError;
}

impl<L: Borrow<GpioLines>> digital::OutputPin for GpioPin<L> {
    fn set_low(&mut self) -> Result<(), IoError> {
        Ok(self.lines.borrow().set(self.offset, false)?)
    }
    fn set_high(&mut self) -> Result<(), IoError> {
        Ok(self.lines.borrow().set(self.offset, true)?)
    }
}

#[cfg(test)]
mod tests {
    use embedded_hal::i2c::Error as _;

    use super::*;

    #[test]
    fn errnos_map_to_i2c_kinds() {
        let k = |e| IoError(io::Error::from_raw_os_error(e)).kind();
        assert_eq!(
            k(libc::ENXIO),
            i2c::ErrorKind::NoAcknowledge(NoAcknowledgeSource::Address)
        );
        assert!(matches!(
            k(libc::EREMOTEIO),
            i2c::ErrorKind::NoAcknowledge(_)
        ));
        assert_eq!(k(libc::EINVAL), i2c::ErrorKind::Other);
        let e = IoError(io::Error::from_raw_os_error(libc::EREMOTEIO));
        assert_eq!(format!("{e:?}"), format!("{}", e.0));
    }
}
