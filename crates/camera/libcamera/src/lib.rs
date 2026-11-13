//! libcamera capture backend for Styx.
//!
//! Enabled by the `libcamera` feature. With it on and libcamera found by pkg-config, cameras are
//! opened through libcamera's own pipeline and 3A (Styx runs no 3A here). Each frame is a
//! libcamera dma-buf handed out as a [`FrameLease`] with no copy. Without the library, or
//! without the feature, every call reports [`Error::Unavailable`] and the build still passes.
//!
//! Frame times are `CLOCK_MONOTONIC` nanoseconds: libcamera stamps `CLOCK_BOOTTIME`, converted per
//! frame (see the backend's `to_monotonic`).

#![deny(clippy::print_stdout)]

use std::fmt;

use styx_controls::FrameMetadata;
use styx_format::FourCc;
use styx_frame::FrameLease;

#[cfg(has_libcamera)]
mod backend;
#[cfg(has_libcamera)]
mod sys;
#[cfg(has_libcamera)]
pub use backend::Capture;

#[cfg(not(has_libcamera))]
mod stub;
#[cfg(not(has_libcamera))]
pub use stub::Capture;

/// Why the backend could not do what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The backend is not compiled in: the feature is off, or libcamera was not found.
    Unavailable(&'static str),
    /// libcamera or the camera refused the request.
    Camera(String),
    /// Styx asked for something this backend does not do.
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(f, "libcamera unavailable: {reason}"),
            Self::Camera(message) => write!(f, "libcamera: {message}"),
            Self::Unsupported(what) => write!(f, "libcamera unsupported: {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// A format a camera offers: a Styx code at a size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    pub code: FourCc,
    pub width: u32,
    pub height: u32,
}

/// A camera libcamera lists, with the formats it offers for the viewfinder role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    pub id: String,
    pub formats: Vec<StreamFormat>,
}

/// What to capture: one format at one size, with `buffers` buffers in the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureConfig {
    pub code: FourCc,
    pub width: u32,
    pub height: u32,
    pub buffers: u32,
}

/// Where frames go: called on libcamera's thread with each frame's lease. Dropping the lease
/// returns the buffer to the camera once the next sweep sees it free.
pub type FrameSink = Box<dyn FnMut(FrameLease<FrameMetadata>) + Send>;

/// Counts since a capture started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureStats {
    /// Frames delivered to the sink.
    pub frames: u64,
    /// Completions whose buffer a consumer still held: dropped, buffer parked.
    pub held: u64,
    /// Completions dropped for another reason.
    pub dropped: u64,
    /// Buffers queued back to libcamera.
    pub requeues: u64,
}

/// Whether the libcamera backend is compiled in and usable on this machine.
pub fn availability() -> Result<(), Error> {
    #[cfg(has_libcamera)]
    {
        Ok(())
    }
    #[cfg(not(has_libcamera))]
    {
        Err(Error::Unavailable(stub::REASON))
    }
}

/// Every camera libcamera lists, with its viewfinder formats.
pub fn list_cameras() -> Result<Vec<CameraInfo>, Error> {
    #[cfg(has_libcamera)]
    {
        backend::list_cameras()
    }
    #[cfg(not(has_libcamera))]
    {
        Err(Error::Unavailable(stub::REASON))
    }
}

#[cfg(not(has_libcamera))]
impl Capture {
    /// The stub's open: always [`Error::Unavailable`].
    pub fn open(_id: &str, _config: CaptureConfig, _sink: FrameSink) -> Result<Self, Error> {
        Err(Error::Unavailable(stub::REASON))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_matches_the_build() {
        #[cfg(has_libcamera)]
        assert!(availability().is_ok());
        #[cfg(not(has_libcamera))]
        match availability() {
            Err(Error::Unavailable(reason)) => assert!(!reason.is_empty()),
            other => panic!("expected unavailable, got {other:?}"),
        }
    }
}
