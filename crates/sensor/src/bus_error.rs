//! The error of a [`RegisterBus`](crate::RegisterBus) or [`SensorPins`](crate::SensorPins)
//! operation, without `std`.

use alloc::borrow::Cow;
use core::fmt;

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
    /// Anything else (the device did not acknowledge, the transfer failed, ...).
    Other,
}

/// The error of a bus or pin operation: a [`BusErrorKind`] and a message. With `std`, an
/// `std::io::Error` converts to and from it losslessly (the I/O error is kept inside, so its
/// errno and message survive a round trip).
pub struct BusError {
    kind: BusErrorKind,
    message: Cow<'static, str>,
    #[cfg(feature = "std")]
    io: Option<std::io::Error>,
}

impl BusError {
    /// An error of `kind` with `message`.
    pub fn new(kind: BusErrorKind, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            kind,
            message: message.into(),
            #[cfg(feature = "std")]
            io: None,
        }
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
            BusErrorKind::Other => K::Other,
        };
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
