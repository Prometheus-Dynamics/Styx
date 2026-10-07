//! The error of a [`RegisterBus`](crate::RegisterBus) or [`SensorPins`](crate::SensorPins)
//! operation, without `std`.

use alloc::borrow::Cow;
use alloc::string::ToString;
use core::fmt;

use lemnos_hal::register::RegisterError;
use styx_hal::{ErrorKind, HalError};

/// What kind of failure a [`BusError`] is: Lemnos's portable kind (`lemnos_hal::ErrorKind`).
/// The driver skips optional power steps whose role is [`NotFound`](Self::NotFound); a
/// register bus without registers (a kernel-driven sensor) and V4L2 controls on a register
/// bus are [`Unsupported`](Self::Unsupported).
pub use lemnos_hal::ErrorKind as BusErrorKind;

/// The error of a bus or pin operation: a [`BusErrorKind`], a message and, where there is one,
/// the platform code (an errno on Linux). With `std`, an `std::io::Error` converts to and from
/// it losslessly (the I/O error is kept inside, so its errno and message survive a round trip).
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

    /// The same error with a platform code (an errno on Linux).
    pub fn with_code(mut self, code: i32) -> Self {
        self.code = Some(code);
        self
    }

    /// The error of a [`styx_hal`] implementation (sensor pins, a lens): its kind, its
    /// platform code and its message.
    pub fn from_hal<E: HalError>(e: &E) -> Self {
        let mut b = Self::new(e.kind().into(), e.to_string());
        b.code = e.code();
        b
    }

    /// The error of a Lemnos register map ([`RegisterBus`](crate::RegisterBus)): Lemnos's
    /// classification and its message. A bus error that already is a [`BusError`] is passed
    /// through; one that is an `std::io::Error` (with `std`) is kept inside, so its errno
    /// survives (map a Linux bus's error to one first, [`RegisterError::map_bus`]).
    pub fn from_register<E: fmt::Debug + 'static>(e: RegisterError<E>) -> Self {
        let kind = lemnos_hal::HalError::kind(&e);
        let RegisterError::Bus { error, .. } = e else {
            return Self::new(kind, misuse(&e));
        };
        let mut error = Some(error);
        let any = &mut error as &mut dyn core::any::Any;
        if let Some(b) = any
            .downcast_mut::<Option<BusError>>()
            .and_then(Option::take)
        {
            return b;
        }
        #[cfg(feature = "std")]
        if let Some(io) = any
            .downcast_mut::<Option<std::io::Error>>()
            .and_then(Option::take)
        {
            return Self {
                kind,
                message: Cow::Borrowed(""),
                code: None,
                io: Some(io),
            };
        }
        let Some(error) = error else {
            return Self::new(kind, "register bus");
        };
        Self::new(kind, alloc::format!("register bus {kind}: {error:?}"))
    }

    /// The error of an embedded-hal I²C bus (classified by Lemnos).
    pub fn from_i2c<E: embedded_hal::i2c::Error>(e: &E) -> Self {
        Self::new(
            BusErrorKind::from_i2c(e.kind()),
            alloc::format!("I2C: {e:?}"),
        )
    }

    /// The error of an embedded-hal SPI device (classified by Lemnos).
    pub fn from_spi<E: embedded_hal::spi::Error>(e: &E) -> Self {
        Self::new(
            BusErrorKind::from_spi(e.kind()),
            alloc::format!("SPI: {e:?}"),
        )
    }

    /// A platform code (an errno on Linux), if known.
    pub fn code(&self) -> Option<i32> {
        #[cfg(feature = "std")]
        if let Some(io) = &self.io {
            return io.raw_os_error();
        }
        self.code
    }

    /// An error of kind [`BusErrorKind::Failed`].
    pub fn other(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(BusErrorKind::Failed, message)
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

/// What a register error that is not the bus's own says (static: no formatting code in a
/// firmware that never fails; the driver's error names the register).
fn misuse<E>(e: &RegisterError<E>) -> &'static str {
    match e {
        RegisterError::InvalidWidth(_) => "register access of more than 4 bytes",
        RegisterError::ValueTooWide { .. } => "value wider than its registers",
        RegisterError::AddressTooWide(_) => "register address wider than the address width",
        RegisterError::TooLong(_) => "register transfer too long",
        RegisterError::Mismatch { .. } => "register read back a different value",
        _ => "register access failed",
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
            .field("code", &self.code)
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
        self.kind.into()
    }
    fn code(&self) -> Option<i32> {
        BusError::code(self)
    }
}

impl lemnos_hal::HalError for BusError {
    fn kind(&self) -> BusErrorKind {
        self.kind
    }
}

#[cfg(feature = "std")]
impl From<std::io::Error> for BusError {
    fn from(io: std::io::Error) -> Self {
        let kind = match io.raw_os_error() {
            Some(errno) => BusErrorKind::from_errno(errno),
            None => BusErrorKind::from_io(io.kind()),
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
        if let Some(code) = e.code {
            return std::io::Error::from_raw_os_error(code);
        }
        let kind = match e.kind {
            BusErrorKind::NotFound => K::NotFound,
            BusErrorKind::Unsupported => K::Unsupported,
            BusErrorKind::InvalidInput => K::InvalidInput,
            BusErrorKind::Timeout => K::TimedOut,
            BusErrorKind::Busy => K::ResourceBusy,
            BusErrorKind::PermissionDenied => K::PermissionDenied,
            _ => K::Other,
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
        assert_eq!(e.kind(), BusErrorKind::Nack);
        assert_eq!(e.code(), Some(121));
        assert_eq!(std::io::Error::from(e).raw_os_error(), Some(121));
        let e = BusError::new(BusErrorKind::NotFound, "no gpio 'reset'");
        assert_eq!(e.to_string(), "no gpio 'reset'");
        let io = std::io::Error::from(e);
        assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(io.to_string(), "no gpio 'reset'");
    }

    #[test]
    fn register_errors_keep_kind_and_errno() {
        let e = BusError::from_register(RegisterError::bus(
            BusErrorKind::Nack,
            std::io::Error::from_raw_os_error(121),
        ));
        assert_eq!((e.kind(), e.code()), (BusErrorKind::Nack, Some(121)));
        assert_eq!(HalError::kind(&e), ErrorKind::Nack);
        let e = BusError::from_register(RegisterError::bus(
            BusErrorKind::Failed,
            BusError::new(BusErrorKind::Busy, "owned by a kernel driver"),
        ));
        assert_eq!(e.kind(), BusErrorKind::Busy);
        assert_eq!(e.to_string(), "owned by a kernel driver");
        let e = BusError::from_register(RegisterError::<()>::InvalidWidth(5));
        assert_eq!(e.kind(), BusErrorKind::InvalidInput);
        assert_eq!(HalError::kind(&e), ErrorKind::InvalidConfig);
    }
}
