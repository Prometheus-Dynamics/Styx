//! The crate error type: which kernel call failed and with what errno.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// An error from a kernel interface.
#[derive(Debug)]
pub enum Error {
    /// An ioctl failed. `name` is the ioctl's uAPI name (e.g. `VIDIOC_S_FMT`).
    Ioctl {
        /// The ioctl's uAPI name.
        name: &'static str,
        /// The errno it returned.
        errno: i32,
    },
    /// A control ioctl (`VIDIOC_S_EXT_CTRLS`, ...) failed on a specific control.
    Control {
        /// The ioctl's uAPI name.
        name: &'static str,
        /// The errno it returned.
        errno: i32,
        /// The id of the control the kernel reported as failing.
        control: u32,
    },
    /// Opening a device node failed.
    Open {
        /// The path that was opened.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// Another system call failed (`mmap`, `poll`, reading sysfs, ...).
    Sys {
        /// The call that failed.
        call: &'static str,
        /// The underlying error.
        source: io::Error,
    },
    /// The kernel returned data this crate cannot interpret, or an argument was invalid.
    Invalid(String),
}

impl Error {
    /// The errno of the failed call, when there is one.
    pub fn errno(&self) -> Option<i32> {
        match self {
            Error::Ioctl { errno, .. } | Error::Control { errno, .. } => Some(*errno),
            Error::Open { source, .. } | Error::Sys { source, .. } => source.raw_os_error(),
            Error::Invalid(_) => None,
        }
    }

    /// True when the call failed with `EINVAL` (commonly: "not supported" or "end of list").
    pub fn is_invalid_argument(&self) -> bool {
        self.errno() == Some(libc::EINVAL)
    }

    /// True when the call failed with `ENOTTY` (the ioctl is not implemented by the driver).
    pub fn is_not_supported(&self) -> bool {
        self.errno() == Some(libc::ENOTTY)
    }

    /// True when the call would have blocked (`EAGAIN`).
    pub fn is_would_block(&self) -> bool {
        self.errno() == Some(libc::EAGAIN)
    }

    /// True when the device went away (`ENODEV`), e.g. a USB camera was unplugged.
    pub fn is_no_device(&self) -> bool {
        self.errno() == Some(libc::ENODEV)
    }

    pub(crate) fn ioctl(name: &'static str, errno: i32) -> Self {
        Error::Ioctl { name, errno }
    }

    pub(crate) fn sys(call: &'static str) -> Self {
        Error::Sys {
            call,
            source: io::Error::last_os_error(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Ioctl { name, errno } => {
                write!(f, "{name} failed: {}", io::Error::from_raw_os_error(*errno))
            }
            Error::Control {
                name,
                errno,
                control,
            } => write!(
                f,
                "{name} failed on control {control:#010x}: {}",
                io::Error::from_raw_os_error(*errno)
            ),
            Error::Open { path, source } => write!(f, "opening {}: {source}", path.display()),
            Error::Sys { call, source } => write!(f, "{call} failed: {source}"),
            Error::Invalid(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Open { source, .. } | Error::Sys { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<Error> for io::Error {
    fn from(err: Error) -> Self {
        match err.errno() {
            Some(errno) => io::Error::new(io::Error::from_raw_os_error(errno).kind(), err),
            None => io::Error::new(io::ErrorKind::InvalidData, err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_the_ioctl_and_errno() {
        let err = Error::ioctl("VIDIOC_S_FMT", libc::EBUSY);
        let text = err.to_string();
        assert!(text.starts_with("VIDIOC_S_FMT failed"), "{text}");
        assert_eq!(err.errno(), Some(libc::EBUSY));
        assert!(!err.is_invalid_argument());
        assert!(Error::ioctl("X", libc::EINVAL).is_invalid_argument());
        assert!(Error::ioctl("X", libc::ENOTTY).is_not_supported());
        assert!(Error::ioctl("VIDIOC_DQBUF", libc::ENODEV).is_no_device());
        let io: io::Error = Error::ioctl("X", libc::EAGAIN).into();
        assert_eq!(io.kind(), io::ErrorKind::WouldBlock);
    }
}
