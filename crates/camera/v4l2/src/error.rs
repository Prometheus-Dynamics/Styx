//! The errors of the V4L2 backend.

use std::fmt;

use styx_controls::{Control, ValueError};

/// Why a V4L2 operation failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A V4L2 call failed (open, ioctl, poll, mmap, export).
    Device(styx_lemnos::v4l2::Error),
    /// The device went away (hangup), or a buffer came back that was never queued.
    Lost(String),
    /// The node cannot do what was asked: no capture queue, a layout this crate does not build,
    /// a buffer shorter than a frame.
    Unsupported(String),
    /// The driver chose another format or size than the one asked for.
    FormatRefused { requested: String, got: String },
    /// A value the control does not take.
    Value(ValueError),
    /// The control has no V4L2 control on this node (or no mapping in this crate).
    NoControl(Control),
    /// An I/O error outside the V4L2 calls (monotonic clock, thread creation, dma-buf mapping).
    Io(std::io::Error),
    /// The stream has stopped, so it takes no more commands.
    Stopped,
    /// The node has no control with this id.
    NoDeviceControl(u32),
    /// A passthrough value the device control does not take: read-only, not settable, out of
    /// range, or a control that is mapped to a Styx control (set that one instead).
    DeviceValue(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Device(e) => write!(f, "v4l2: {e}"),
            Error::Lost(why) => write!(f, "v4l2 device lost: {why}"),
            Error::Unsupported(why) => write!(f, "v4l2: {why}"),
            Error::FormatRefused { requested, got } => {
                write!(f, "v4l2 driver refused {requested}, gave {got}")
            }
            Error::Value(e) => write!(f, "v4l2 control value: {e:?}"),
            Error::NoControl(control) => {
                write!(f, "v4l2: no V4L2 control for {}", control.name())
            }
            Error::Io(e) => write!(f, "v4l2: {e}"),
            Error::Stopped => f.write_str("v4l2 stream has stopped"),
            Error::NoDeviceControl(id) => write!(f, "v4l2: no control with id {id:#010x}"),
            Error::DeviceValue(why) => write!(f, "v4l2 device control: {why}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Device(e) => Some(e),
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<styx_lemnos::v4l2::Error> for Error {
    fn from(e: styx_lemnos::v4l2::Error) -> Self {
        Error::Device(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<ValueError> for Error {
    fn from(e: ValueError) -> Self {
        Error::Value(e)
    }
}
