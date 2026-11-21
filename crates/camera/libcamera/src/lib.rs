//! libcamera capture backend for Styx.
//!
//! Enabled by the `libcamera` feature. With it on, cameras are opened through libcamera's own
//! pipeline and 3A (Styx runs no 3A here). Each frame is a libcamera dma-buf handed out as a
//! [`FrameLease`] with no copy. Without the feature every call reports [`Error::Unavailable`].
//!
//! The binding is lit-robotics libcamera-rs: `libcamera-sys` for libcamera's API as C calls, and
//! `libcamera` for the control ids of the installed libcamera. Styx carries no C or C++ for it.
//! The feature needs libcamera's development files found by pkg-config, or the build fails.
//!
//! Multi-plane formats (NV12, I420 and the like) are handed out when a buffer's planes sit in one
//! dma-buf, which is libcamera's usual layout; the plane placement is in `planes`. Planes in
//! several dma-bufs are refused with the format's name, since a styx-frame buffer is one memory.
//!
//! Zero-copy is the design, but it is not yet run on a camera: the proof (a real camera's frames
//! with zero copies, counted) runs later, on a board, in PR H6. Until then the copy counters
//! prove nothing about a camera, and the test in `tests/capture.rs` skips where there is none.
//!
//! Frame times are `CLOCK_MONOTONIC` nanoseconds: libcamera stamps `CLOCK_BOOTTIME`, converted per
//! frame (see the backend's `to_monotonic`).

#![deny(clippy::print_stdout)]

use std::fmt;

use styx_controls::{Control, ControlValue, DeviceControl, FrameMetadata, Rect};
use styx_format::FourCc;
use styx_frame::FrameLease;

// Plane placement and the control mapping are plain arithmetic and tables: they build and are
// tested with or without libcamera.
#[cfg_attr(not(feature = "libcamera"), allow(dead_code))]
mod controls;
#[cfg_attr(not(feature = "libcamera"), allow(dead_code))]
mod planes;

#[cfg(feature = "libcamera")]
mod backend;
#[cfg(feature = "libcamera")]
mod sys;
#[cfg(feature = "libcamera")]
pub use backend::Capture;

#[cfg(not(feature = "libcamera"))]
mod stub;
#[cfg(not(feature = "libcamera"))]
pub use stub::Capture;

/// Why the backend could not do what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The backend is not compiled in: the `libcamera` feature is off.
    Unavailable(&'static str),
    /// libcamera or the camera refused the request.
    Camera(String),
    /// Styx asked for something this backend does not do.
    Unsupported(&'static str),
    /// The camera has no such control, or the control refuses the value it was given.
    Refused(String),
    /// A value the control does not take.
    Value(styx_controls::ValueError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(f, "libcamera unavailable: {reason}"),
            Self::Camera(message) => write!(f, "libcamera: {message}"),
            Self::Unsupported(what) => write!(f, "libcamera unsupported: {what}"),
            Self::Refused(why) => write!(f, "libcamera: {why}"),
            Self::Value(e) => write!(f, "libcamera control value: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<styx_controls::ValueError> for Error {
    fn from(e: styx_controls::ValueError) -> Self {
        Error::Value(e)
    }
}

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

/// A control a camera offers, listed by libcamera's id and name. Every listed control is either
/// mapped to a Styx control or a passthrough: `mapped` says which.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceEntry {
    /// The control: libcamera's id, name, kind and bounds (floats are truncated to integers here;
    /// the backend keeps the float ranges it maps to a Styx control).
    pub control: DeviceControl,
    /// The Styx control this one is used for, when this crate maps it.
    pub mapped: Option<Control>,
    /// libcamera's name for it in full (`control.name` is cut to its inline capacity).
    pub name: String,
    /// A menu's entries: the values the camera's `ControlInfo` lists (every enumerator when it
    /// lists none), labelled with libcamera's enumerator names. Empty for other controls.
    pub menu: Vec<MenuItem>,
    /// The value last set for this control on this capture. `None` until one is set: libcamera
    /// reports what a camera did in each frame's metadata, not through a read.
    pub value: Option<i64>,
    /// A float control's numbers, read at their own type (`f32`). Its integer bounds in `control`
    /// are not meaningful.
    pub real: Option<FloatValues>,
    /// A rectangle control's rectangles.
    pub rect: Option<RectValues>,
}

/// A float control's numbers as libcamera gives them. A bound libcamera does not give, or gives as
/// infinity or NaN, is `None`: no slider is built from infinity. `value` is the one last set.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FloatValues {
    pub min: Option<f32>,
    pub max: Option<f32>,
    pub default: Option<f32>,
    pub value: Option<f32>,
}

/// A rectangle control's rectangles as libcamera gives them. `value` is the one last set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RectValues {
    pub min: Option<Rect>,
    pub max: Option<Rect>,
    pub default: Option<Rect>,
    pub value: Option<Rect>,
}

/// One entry of a libcamera menu control: the value that selects it and libcamera's name for it.
/// `name` is `None` for a value the camera lists that has no enumerator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub value: i64,
    pub name: Option<String>,
}

/// A Styx control as this camera has it, in Styx units, from the ranges libcamera gives after the
/// camera is configured. Only the controls the backend can set are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandardControl {
    pub control: Control,
    /// `None` when the camera gives no range for it (white balance, which is Auto only here).
    pub range: Option<StandardRange>,
    pub default: Option<ControlValue>,
    /// Whether the camera has an automatic mode for it.
    pub auto: bool,
    /// The camera does not take a value for it.
    pub read_only: bool,
}

/// The values a [`StandardControl`] takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandardRange {
    /// From `min` to `max` in the control's Styx unit.
    Manual { min: u32, max: u32 },
    /// On or off.
    Flag,
    /// A rectangle (crop).
    Rect,
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
    #[cfg(feature = "libcamera")]
    {
        Ok(())
    }
    #[cfg(not(feature = "libcamera"))]
    {
        Err(Error::Unavailable(stub::REASON))
    }
}

/// Every camera libcamera lists, with its viewfinder formats.
pub fn list_cameras() -> Result<Vec<CameraInfo>, Error> {
    #[cfg(feature = "libcamera")]
    {
        backend::list_cameras()
    }
    #[cfg(not(feature = "libcamera"))]
    {
        Err(Error::Unavailable(stub::REASON))
    }
}

#[cfg(not(feature = "libcamera"))]
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
        #[cfg(feature = "libcamera")]
        assert!(availability().is_ok());
        #[cfg(not(feature = "libcamera"))]
        match availability() {
            Err(Error::Unavailable(reason)) => assert!(!reason.is_empty()),
            other => panic!("expected unavailable, got {other:?}"),
        }
    }
}
