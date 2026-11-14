//! The one way to read an opened camera, whichever backend owns it.
//!
//! Frames are [`FrameLease`]s with [`FrameMetadata`], controls are [`Update`]s, and status is
//! [`Status`]. The adapters in [`crate::adapters`] put the three backends' streams behind this
//! trait without changing those backends.

use std::fmt;
use std::time::Duration;

use styx_controls::{FrameMetadata, Update};
use styx_lemnos::LayoutRefusal;
use styx_frame::FrameLease;

/// What an opened camera is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Frames flow.
    Streaming,
    /// The device failed; the backend is retrying. `reason` is its last error. `layout` is the
    /// frame stream's shared-layout refusal when that is the cause, as its values.
    Retrying {
        reason: String,
        layout: Option<LayoutRefusal>,
    },
    /// Stopped by the caller.
    Stopped,
    /// The backend gave up. `layout` as for [`Status::Retrying`].
    Failed {
        reason: String,
        layout: Option<LayoutRefusal>,
    },
}

/// Why a capture call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// The camera was stopped.
    Stopped,
    /// The backend failed (its message).
    Failed(String),
    /// The backend does not do what was asked; the name of the missing operation.
    Unsupported(&'static str),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::Stopped => f.write_str("camera stopped"),
            CaptureError::Failed(message) => f.write_str(message),
            CaptureError::Unsupported(what) => write!(f, "unsupported by this backend: {what}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// What the backend says about the frame rate its camera runs at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RateReport {
    /// The rate the frames run at, in thousandths of a frame per second, as the backend knows it
    /// (native: the frame length over the line time). `None` when the backend does not know.
    pub achieved_milli: Option<u32>,
    /// The backend clamped the rate asked for to the sensor mode's limit.
    pub clamped: bool,
}

/// An opened camera. Dropping it stops it.
pub trait Capture {
    /// The next frame, or `None` when none arrived within `timeout`.
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<FrameLease<FrameMetadata>>, CaptureError>;

    /// Applies controls. A backend that applies a value on the next frame it takes does so;
    /// `Update::frame` is honoured where the backend supports frame-exact landing and otherwise
    /// ignored (see each adapter).
    fn set_controls(&mut self, updates: &[Update]) -> Result<(), CaptureError>;

    /// What the camera is doing now.
    fn status(&self) -> Status;

    /// The frame rate the backend reports. The default says nothing is known; a backend that
    /// knows its rate overrides it.
    fn rate(&self) -> RateReport {
        RateReport::default()
    }

    /// Frames the reader was too late for, in all (a Lemnos stream's `skipped`, on a native
    /// camera): frames the backend made that this reader never saw. 0 where the backend has no such
    /// count.
    fn lemnos_skipped(&self) -> u64 {
        0
    }

    /// Stops the camera. Idempotent.
    fn stop(&mut self);
}
