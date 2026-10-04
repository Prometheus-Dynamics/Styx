//! What kind of failure a hardware operation had, whatever the platform's own error type.

use core::fmt;

/// The kind of a hardware failure. Implementations keep their own error types (an errno on
/// Linux, a vendor status on a microcontroller) and say which kind it is through
/// [`HalError::kind`], so the runtime can decide (skip an optional power step, end a stream on
/// a disconnect, count a glitch) without knowing the platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The device is gone (`ENODEV`, a USB unplug, a bus that stopped answering).
    Disconnected,
    /// Owned by someone else.
    Busy,
    /// The OS or the platform refused access.
    PermissionDenied,
    /// The device did not answer in time.
    Timeout,
    /// An I²C address or data byte was not acknowledged.
    Nack,
    /// A role or resource the board does not have (optional power steps are skipped).
    NotFound,
    /// The operation is not available here.
    Unsupported,
    /// The arguments or the configuration do not fit the hardware.
    InvalidConfig,
    /// Out of (DMA-capable) memory.
    NoMemory,
    /// The receiver had no buffer, or a FIFO overflowed: a frame is lost, the stream goes on.
    Overrun,
    /// A sync or CRC error on a frame: the frame is flagged, the stream goes on.
    Corrupt,
    /// Any other I/O failure.
    Io,
    /// Anything else.
    Other,
}

impl ErrorKind {
    /// A short description.
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Disconnected => "device disconnected",
            ErrorKind::Busy => "device busy",
            ErrorKind::PermissionDenied => "permission denied",
            ErrorKind::Timeout => "timed out",
            ErrorKind::Nack => "not acknowledged",
            ErrorKind::NotFound => "not found",
            ErrorKind::Unsupported => "not supported",
            ErrorKind::InvalidConfig => "invalid configuration",
            ErrorKind::NoMemory => "out of memory",
            ErrorKind::Overrun => "overrun",
            ErrorKind::Corrupt => "corrupt frame",
            ErrorKind::Io => "I/O error",
            ErrorKind::Other => "error",
        }
    }

    /// The kind of an embedded-hal I²C error (classified by Lemnos).
    pub fn from_i2c(kind: embedded_hal::i2c::ErrorKind) -> Self {
        lemnos_hal::ErrorKind::from_i2c(kind).into()
    }

    /// The kind of an embedded-hal SPI error (classified by Lemnos).
    pub fn from_spi(kind: embedded_hal::spi::ErrorKind) -> Self {
        lemnos_hal::ErrorKind::from_spi(kind).into()
    }

    /// The kind of an embedded-hal digital (GPIO) error.
    pub fn from_digital(kind: embedded_hal::digital::ErrorKind) -> Self {
        lemnos_hal::ErrorKind::from_digital(kind).into()
    }

    /// The kind of a `std::io::Error`: its errno classified by Lemnos (`ENXIO`/`EREMOTEIO`
    /// are a NACK, `ENODEV` a disconnect, ...), else its I/O kind.
    #[cfg(feature = "std")]
    pub fn from_io(e: &std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::OutOfMemory {
            return ErrorKind::NoMemory;
        }
        match e.raw_os_error() {
            Some(errno) => lemnos_hal::ErrorKind::from_errno(errno).into(),
            None => lemnos_hal::ErrorKind::from_io(e.kind()).into(),
        }
    }
}

/// Lemnos's portable kinds (buses, pins, regulators, clocks) as camera hardware kinds.
impl From<lemnos_hal::ErrorKind> for ErrorKind {
    fn from(kind: lemnos_hal::ErrorKind) -> Self {
        use lemnos_hal::ErrorKind as L;
        match kind {
            L::NotFound => ErrorKind::NotFound,
            L::Unavailable => ErrorKind::Disconnected,
            L::Busy => ErrorKind::Busy,
            L::PermissionDenied => ErrorKind::PermissionDenied,
            L::Timeout => ErrorKind::Timeout,
            L::InvalidInput | L::Configuration => ErrorKind::InvalidConfig,
            L::Unsupported => ErrorKind::Unsupported,
            L::Nack => ErrorKind::Nack,
            L::Overrun => ErrorKind::Overrun,
            L::Failed => ErrorKind::Io,
            _ => ErrorKind::Other,
        }
    }
}

/// The closest Lemnos kind (for Lemnos traits implemented over Styx errors).
impl From<ErrorKind> for lemnos_hal::ErrorKind {
    fn from(kind: ErrorKind) -> Self {
        use lemnos_hal::ErrorKind as L;
        match kind {
            ErrorKind::Disconnected => L::Unavailable,
            ErrorKind::Busy => L::Busy,
            ErrorKind::PermissionDenied => L::PermissionDenied,
            ErrorKind::Timeout => L::Timeout,
            ErrorKind::Nack => L::Nack,
            ErrorKind::NotFound => L::NotFound,
            ErrorKind::Unsupported => L::Unsupported,
            ErrorKind::InvalidConfig => L::InvalidInput,
            ErrorKind::Overrun => L::Overrun,
            ErrorKind::NoMemory | ErrorKind::Corrupt | ErrorKind::Io | ErrorKind::Other => {
                L::Failed
            }
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl core::error::Error for ErrorKind {}

/// An error of a hardware trait implementation: its [`ErrorKind`] and, where there is one, a
/// platform code for logs (an errno on Linux, a vendor status on microcontrollers). `Display`
/// is its message.
pub trait HalError: fmt::Debug + fmt::Display {
    /// What kind of failure this is.
    fn kind(&self) -> ErrorKind;
    /// A platform code, if any.
    fn code(&self) -> Option<i32> {
        None
    }
}

impl HalError for ErrorKind {
    fn kind(&self) -> ErrorKind {
        *self
    }
}

impl HalError for core::convert::Infallible {
    fn kind(&self) -> ErrorKind {
        match *self {}
    }
}

/// Lemnos's own kind (a regulator, clock or pin error) is a camera hardware error.
impl HalError for lemnos_hal::ErrorKind {
    fn kind(&self) -> ErrorKind {
        (*self).into()
    }
}

#[cfg(feature = "std")]
impl HalError for std::io::Error {
    fn kind(&self) -> ErrorKind {
        ErrorKind::from_io(self)
    }
    fn code(&self) -> Option<i32> {
        self.raw_os_error()
    }
}
