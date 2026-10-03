//! USB Video Class (UVC 1.0–1.5) cameras from userspace, over Linux usbfs: no `uvcvideo`, no
//! libusb, no C.
//!
//! - [`enumerate`] finds cameras in sysfs (no device access needed) and parses their
//!   descriptors ([`descriptors`]): units and terminals, formats (YUY2, NV12, MJPEG,
//!   frame-based), frame sizes and intervals, alternate settings.
//! - [`UvcDevice::open`] claims the camera's video interfaces through usbfs (optionally
//!   detaching `uvcvideo` from just those, [`OpenOptions::detach_kernel_driver`]), and drives
//!   its controls ([`controls`], V4L2 ids and units, as `uvcvideo` maps them).
//! - [`UvcDevice::start`] negotiates (PROBE/COMMIT), picks the alternate setting by
//!   bandwidth, and streams isochronous or bulk transfers; [`UvcStream`] assembles frames from
//!   the payloads ([`payload`]) straight into pooled buffers and timestamps them: the capture
//!   time from PTS through SCR ([`clock`]), and each payload's bus arrival time.
//! - [`Hotplug`] follows cameras coming and going (kernel uevents).
//!
//! The stream is driven by its owner: [`UvcStream::next`] (async, any executor),
//! [`UvcStream::next_blocking`], or [`UvcStream::try_next`] with the stream's descriptor in
//! your own poll loop. See `docs/uvc.md`.

#![cfg(target_os = "linux")]

pub mod clock;
pub mod controls;
pub mod descriptors;
mod device;
pub mod hotplug;
pub mod payload;
pub mod pool;
pub mod probe;
mod stream;
pub mod sysfs;

pub use controls::{ControlDef, ControlInfo};
pub use descriptors::{Format, FormatKind, Frame, Intervals, StreamingInterface, UvcFunction};
pub use device::{OpenOptions, UvcDevice, find_mode};
pub use hotplug::{Hotplug, HotplugEvent};
pub use payload::{FrameFlags, Scr};
pub use stream::{StreamConfig, StreamFormat, StreamStats, UvcFrame, UvcStream};
pub use sysfs::{UsbCameraInfo, enumerate};

/// Errors of this crate.
#[derive(Debug, thiserror::Error)]
pub enum UvcError {
    #[error("descriptors: {0}")]
    Descriptor(String),
    #[error("{0}")]
    Kernel(styx_kernel::Error),
    /// A kernel driver holds one of the camera's interfaces.
    #[error(
        "interface {interface} is bound to the kernel driver {driver} (unbind it, or open with detach_kernel_driver)"
    )]
    KernelDriver { interface: u8, driver: String },
    #[error("busy: {0}")]
    Busy(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("invalid: {0}")]
    Invalid(String),
    /// The camera went away.
    #[error("camera disconnected")]
    Disconnected,
    #[error("timed out")]
    Timeout,
}

impl UvcError {
    /// Maps a kernel error, recognising a vanished device.
    pub fn from_kernel(e: styx_kernel::Error) -> UvcError {
        match e.errno() {
            Some(libc::ENODEV | libc::ESHUTDOWN) => UvcError::Disconnected,
            Some(libc::ENOENT) if matches!(e, styx_kernel::Error::Open { .. }) => {
                UvcError::Disconnected
            }
            _ => UvcError::Kernel(e),
        }
    }

    /// Whether the camera went away.
    pub fn is_disconnect(&self) -> bool {
        matches!(self, UvcError::Disconnected)
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, UvcError>;

/// A frame interval in 100 ns units as a reduced fraction of seconds (`333333` → 1/30), the
/// way `uvcvideo` reports intervals to V4L2.
pub fn interval_fraction(interval: u32) -> (u32, u32) {
    // uvcvideo's uvc_simplify_fraction: 8 continued-fraction terms, stop at a term >= 333.
    let (mut x, mut y) = (u64::from(interval), 10_000_000u64);
    let mut an = [0u64; 8];
    let mut n = 0;
    while n < an.len() && y != 0 {
        an[n] = x / y;
        if an[n] >= 333 {
            if n < 2 {
                n += 1;
            }
            break;
        }
        (x, y) = (y, x - an[n] * y);
        n += 1;
    }
    let (mut x, mut y) = (0u64, 1u64);
    for i in (1..=n).rev() {
        (x, y) = (y, an[i - 1] * y + x);
    }
    if x == 0 || y == 0 || x > u64::from(u32::MAX) || y > u64::from(u32::MAX) {
        return (interval.max(1), 10_000_000);
    }
    (y as u32, x as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_reduce_like_uvcvideo() {
        assert_eq!(interval_fraction(333_333), (1, 30));
        assert_eq!(interval_fraction(666_666), (1, 15));
        assert_eq!(interval_fraction(500_000), (1, 20));
        assert_eq!(interval_fraction(1_000_000), (1, 10));
        assert_eq!(interval_fraction(2_000_000), (1, 5));
        assert_eq!(interval_fraction(10_000_000), (1, 1));
        assert_eq!(interval_fraction(416_666), (1, 24));
        assert_eq!(interval_fraction(166_666), (1, 60));
    }
}
