//! The one way to read an opened camera, whichever backend owns it.
//!
//! Frames are [`FrameLease`]s with [`FrameMetadata`], controls are [`Update`]s, and status is
//! [`Status`]. The adapters in [`crate::adapters`] put the three backends' streams behind this
//! trait without changing those backends.

use std::fmt;
use std::time::Duration;

use styx_controls::{FrameMetadata, Update};
use styx_frame::FrameLease;
use styx_lemnos::{Carried, LayoutRefusal};

use crate::controls::ControlsInfo;

/// What an opened camera is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Frames flow.
    Streaming,
    /// The device failed; the backend is retrying. `reason` is its last error. `layout` is the
    /// frame stream's shared-layout refusal when that is the cause, as its values. `contract` is
    /// the device contract the board's Lemnos lays out another way, when that is the cause.
    Retrying {
        reason: String,
        layout: Option<LayoutRefusal>,
        contract: Option<String>,
    },
    /// Stopped by the caller.
    Stopped,
    /// The backend gave up. `layout` and `contract` as for [`Status::Retrying`].
    Failed {
        reason: String,
        layout: Option<LayoutRefusal>,
        contract: Option<String>,
    },
}

/// A change in the camera between two frames, which a caller hears of through
/// [`Capture::take_edge`]. A stall is health: the camera is retried and the frame stream stays
/// attached, so the caller's readers stay attached too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edge {
    /// The camera failed and is being retried since `since_ns` (CLOCK_MONOTONIC nanoseconds). `text`
    /// is the status line: `camera failed, retrying since <ns>: <reason>`.
    Stalled { since_ns: u64, text: String },
    /// The camera is back. `Carried::Kept`: the same device carried on. `Carried::Lost`: the device
    /// was bound again and the frame sequence restarted, a discontinuity rather than a gap.
    Resumed { carried: Carried },
}

/// Why a capture call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// The camera was stopped.
    Stopped,
    /// The backend failed (its message).
    Failed(String),
    /// The camera could not start because a device's contract is another build's than this
    /// Styx's: `contract` names it and `text` is the failure's sentence. Retried like any failure.
    Contract { contract: String, text: String },
    /// The backend does not do what was asked; the name of the missing operation.
    Unsupported(&'static str),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::Stopped => f.write_str("camera stopped"),
            CaptureError::Failed(message) | CaptureError::Contract { text: message, .. } => {
                f.write_str(message)
            }
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

    /// Reads that found this reader at its declared hold, in all (a Lemnos stream's `AtLimit`, on a
    /// native camera): no frame was read because the reader held every buffer it declared. 0 where
    /// the backend has no such count.
    fn lemnos_at_limit(&self) -> u64 {
        0
    }

    /// The buffers of the camera's pool this reader declared it holds at once, every lease and
    /// share of its frames included (a native camera's Lemnos stream); `None` where the backend
    /// bounds no hold. A frame in hand is one of them, so whatever keeps frames past it (a ring of
    /// recent frames for late readers) fits in one fewer.
    fn hold(&self) -> Option<u32> {
        None
    }

    /// The change in the camera since the last call, if one came with the last `next_frame`
    /// (which then returned no frame). A backend without stalls says nothing.
    fn take_edge(&mut self) -> Option<Edge> {
        None
    }

    /// The camera's controls as it has them: the standard ones with this camera's ranges, and every
    /// device control ([`ControlsInfo`]). Built on each call, so a caller asks when
    /// [`Capture::controls_generation`] moves, not per frame. The default is a backend that does not
    /// report its controls: every standard control, nothing known.
    fn controls_info(&self) -> ControlsInfo {
        ControlsInfo::unreported()
    }

    /// A count that moves whenever [`Capture::controls_info`] would answer differently: a range that
    /// changed (a frame rate moving exposure's ceiling), a value set by id. Cheap: a caller may read
    /// it every frame. The default never moves.
    fn controls_generation(&self) -> u64 {
        0
    }

    /// Sets device control `id` ([`crate::DeviceInfo::id`]) to `value`, in the device's own units.
    /// The default is a backend without device controls.
    fn set_device(&mut self, id: u32, value: i64) -> Result<(), CaptureError> {
        let _ = (id, value);
        Err(CaptureError::Unsupported("device controls"))
    }

    /// Sets [`styx_controls::DeviceKind::Float`] device control `id` to `value`. The default is a
    /// backend without real device controls.
    fn set_device_real(&mut self, id: u32, value: f32) -> Result<(), CaptureError> {
        let _ = (id, value);
        Err(CaptureError::Unsupported("real device controls"))
    }

    /// Stops the camera. Idempotent.
    fn stop(&mut self);
}
