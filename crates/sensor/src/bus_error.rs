//! The error of a [`RegisterBus`](crate::RegisterBus) or [`SensorPins`](crate::SensorPins)
//! operation, without `std`.

use alloc::borrow::Cow;
use alloc::format;
use alloc::string::ToString;
use core::fmt;

use styx_hal::{ErrorKind, HalError};

/// What kind of failure a [`BusError`] is. The driver skips optional power steps whose role
/// is [`NotFound`](Self::NotFound).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BusErrorKind {
    /// The role (GPIO, clock, supply) or device does not exist.
    NotFound,
    /// The operation is not available on this bus (e.g. registers of a sensor a kernel
    /// driver owns, V4L2 controls on a register bus).
    Unsupported,
    /// The arguments do not fit (register width, value range, clock rate).
    InvalidInput,
    /// The device did not answer in time.
    TimedOut,
    /// An I²C address or data byte was not acknowledged (the sensor did not answer).
    Nack,
    /// Anything else (the device did not acknowledge, the transfer failed, ...).
    Other,
}

/// The error of a bus or pin operation: a [`BusErrorKind`] and a message. With `std`, an
/// `std::io::Error` converts to and from it losslessly (the I/O error is kept inside, so its
/// errno and message survive a round trip).
pub struct BusError {
    kind: BusErrorKind,
    message: Cow<'static, str>,
    code: Option<i32>,
    #[cfg(feature = "std")]
    io: Option<std::io::Error>,
}

impl BusError {
    /// An error of `kind` with `message`.
    pub fn new(kind: BusErrorKind, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            kind,
            message: message.into(),
            code: None,
            #[cfg(feature = "std")]
            io: None,
        }
    }

    /// The error of a [`styx_hal`] implementation (sensor pins, a lens): its kind, its
    /// platform code and its message.
    pub fn from_hal<E: HalError>(e: &E) -> Self {
        let kind = match e.kind() {
            ErrorKind::NotFound => BusErrorKind::NotFound,
            ErrorKind::Unsupported => BusErrorKind::Unsupported,
            ErrorKind::InvalidConfig => BusErrorKind::InvalidInput,
            ErrorKind::Timeout => BusErrorKind::TimedOut,
            ErrorKind::Nack => BusErrorKind::Nack,
            _ => BusErrorKind::Other,
        };
        let mut b = Self::new(kind, e.to_string());
        b.code = e.code();
        b
    }

    /// The error of an embedded-hal I²C bus.
    pub fn from_i2c<E: embedded_hal::i2c::Error>(e: &E) -> Self {
        use embedded_hal::i2c::ErrorKind as K;
        let kind = match e.kind() {
            K::NoAcknowledge(_) => BusErrorKind::Nack,
            _ => BusErrorKind::Other,
        };
        Self::new(kind, format!("I2C: {e:?}"))
    }

    /// The error of an embedded-hal SPI device.
    pub fn from_spi<E: embedded_hal::spi::Error>(e: &E) -> Self {
        Self::other(format!("SPI: {e:?}"))
    }

    /// A platform code (an errno on Linux), if known.
    pub fn code(&self) -> Option<i32> {
        #[cfg(feature = "std")]
        if let Some(io) = &self.io {
            return io.raw_os_error();
        }
        self.code
    }

    /// An error of kind [`BusErrorKind::Other`].
    pub fn other(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(BusErrorKind::Other, message)
    }

    /// The kind of failure.
    pub fn kind(&self) -> BusErrorKind {
        self.kind
    }

    /// The I/O error this one was made from, if any.
    #[cfg(feature = "std")]
    pub fn io_error(&self) -> Option<&std::io::Error> {
        self.io.as_ref()
    }
}

impl fmt::Debug for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(feature = "std")]
        if let Some(io) = &self.io {
            return fmt::Debug::fmt(io, f);
        }
        f.debug_struct("BusError")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .finish()
    }
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(feature = "std")]
        if let Some(io) = &self.io {
            return fmt::Display::fmt(io, f);
        }
        f.write_str(&self.message)
    }
}

impl core::error::Error for BusError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        #[cfg(feature = "std")]
        if let Some(io) = &self.io {
            return io.source();
        }
        None
    }
}

impl HalError for BusError {
    fn kind(&self) -> ErrorKind {
        match self.kind {
            BusErrorKind::NotFound => ErrorKind::NotFound,
            BusErrorKind::Unsupported => ErrorKind::Unsupported,
            BusErrorKind::InvalidInput => ErrorKind::InvalidConfig,
            BusErrorKind::TimedOut => ErrorKind::Timeout,
            BusErrorKind::Nack => ErrorKind::Nack,
            BusErrorKind::Other => ErrorKind::Io,
        }
    }
    fn code(&self) -> Option<i32> {
        BusError::code(self)
    }
}

#[cfg(feature = "std")]
impl From<std::io::Error> for BusError {
    fn from(io: std::io::Error) -> Self {
        use std::io::ErrorKind as K;
        let kind = match io.kind() {
            K::NotFound => BusErrorKind::NotFound,
            K::Unsupported => BusErrorKind::Unsupported,
            K::InvalidInput => BusErrorKind::InvalidInput,
            K::TimedOut => BusErrorKind::TimedOut,
            _ => BusErrorKind::Other,
        };
        Self {
            kind,
            message: Cow::Borrowed(""),
            code: None,
            io: Some(io),
        }
    }
}

#[cfg(feature = "std")]
impl From<BusError> for std::io::Error {
    fn from(e: BusError) -> Self {
        use std::io::ErrorKind as K;
        if let Some(io) = e.io {
            return io;
        }
        let kind = match e.kind {
            BusErrorKind::NotFound => K::NotFound,
            BusErrorKind::Unsupported => K::Unsupported,
            BusErrorKind::InvalidInput => K::InvalidInput,
            BusErrorKind::TimedOut => K::TimedOut,
            BusErrorKind::Nack | BusErrorKind::Other => K::Other,
        };
        if let Some(code) = e.code {
            return std::io::Error::from_raw_os_error(code);
        }
        std::io::Error::new(kind, e.message.into_owned())
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn io_errors_round_trip() {
        let e = BusError::from(std::io::Error::from_raw_os_error(121));
        assert_eq!(e.kind(), BusErrorKind::Other);
        assert_eq!(std::io::Error::from(e).raw_os_error(), Some(121));
        let e = BusError::new(BusErrorKind::NotFound, "no gpio 'reset'");
        assert_eq!(e.to_string(), "no gpio 'reset'");
        let io = std::io::Error::from(e);
        assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(io.to_string(), "no gpio 'reset'");
    }
}
