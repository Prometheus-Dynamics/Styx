//! Putting every camera's timestamps on one clock.

use std::time::{Duration, Instant};

use styx_core::prelude::*;

/// Which clock a [`FrameGrouper`](super::FrameGrouper) compares timestamps on.
///
/// Backends stamp frames on different clocks (docs/timestamps.md): libcamera on
/// `CLOCK_BOOTTIME` (its `SensorTimestamp`, start of exposure), V4L2 and native captures on
/// `CLOCK_MONOTONIC` (the receiver's frame start), netcam and virtual sources on the clock
/// [`ClockSource`] chose (stream-relative by default), files and simulation on media time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockMode {
    /// Convert every timestamp to this system clock (the offset between two system clocks is
    /// sampled once a second; monotonic and boottime differ only by time spent suspended).
    /// Frames whose clock is unknown, or stream-relative, cannot be converted: they are
    /// dropped ([`DropReason::Clock`](super::DropReason::Clock)) and
    /// [`FrameGrouper::last_error`](super::FrameGrouper::last_error) says why.
    Common(TimestampClock),
    /// When Styx took each frame from its driver ([`Hop::Dequeued`], `CLOCK_MONOTONIC`, carried
    /// across processes with the frame; else when the frame reached the grouper), not the
    /// sensor timestamp: for sources without a sensor clock (virtual, netcam, files). The spread
    /// then includes each camera's delivery jitter.
    Arrival,
    /// Compare timestamps as they are, for sources that share a clock Styx cannot name (one
    /// stream-relative timeline, a hardware trigger counter, a replay). Every frame must report
    /// the same `FrameMeta::clock` as the first one did.
    Raw,
}

impl Default for ClockMode {
    fn default() -> Self {
        Self::Common(TimestampClock::Monotonic)
    }
}

/// Why a frame's timestamp could not be put on the grouper's clock.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClockError {
    #[error("camera `{camera}`: frame has no timestamp clock, so it cannot be compared")]
    Unknown { camera: String },
    #[error(
        "camera `{camera}`: {clock:?} timestamps cannot be converted to {target:?} \
         (use ClockMode::Raw when every camera shares that timeline, or stamp the source on a \
         system clock with StyxConfig::timestamp_clock)"
    )]
    Unrelated {
        camera: String,
        clock: TimestampClock,
        target: TimestampClock,
    },
    #[error("camera `{camera}`: clock {got:?} differs from the {expected:?} of the first frame")]
    Mismatch {
        camera: String,
        expected: Option<TimestampClock>,
        got: Option<TimestampClock>,
    },
}

/// How long a sampled offset between two system clocks is used.
const RESAMPLE: Duration = Duration::from_secs(1);

pub(super) struct ClockMap {
    mode: ClockMode,
    /// `Raw`: the clock of the first frame.
    raw: Option<Option<TimestampClock>>,
    conversions: Vec<(TimestampClock, ClockConversion, Instant)>,
}

impl ClockMap {
    pub(super) fn new(mode: ClockMode) -> Self {
        Self {
            mode,
            raw: None,
            conversions: Vec::new(),
        }
    }

    pub(super) fn mode(&self) -> ClockMode {
        self.mode
    }

    /// `meta`'s timestamp on the grouper's clock.
    pub(super) fn map(&mut self, camera: &str, meta: &FrameMeta) -> Result<u64, ClockError> {
        let target = match self.mode {
            ClockMode::Raw => {
                let expected = *self.raw.get_or_insert(meta.clock);
                return if expected == meta.clock {
                    Ok(meta.timestamp)
                } else {
                    Err(ClockError::Mismatch {
                        camera: camera.into(),
                        expected,
                        got: meta.clock,
                    })
                };
            }
            ClockMode::Arrival => {
                return Ok(meta
                    .hops
                    .get(Hop::Dequeued)
                    .or(meta.capture_instant.map(CaptureInstant::as_nanos))
                    .unwrap_or_else(|| CaptureInstant::now().as_nanos()));
            }
            ClockMode::Common(target) => target,
        };
        let clock = meta.clock.ok_or_else(|| ClockError::Unknown {
            camera: camera.into(),
        })?;
        if clock == target {
            return Ok(meta.timestamp);
        }
        let unrelated = || ClockError::Unrelated {
            camera: camera.into(),
            clock,
            target,
        };
        if clock == TimestampClock::StreamRelative || target == TimestampClock::StreamRelative {
            return Err(unrelated());
        }
        let now = Instant::now();
        let at = match self.conversions.iter().position(|(c, ..)| *c == clock) {
            Some(at) if now.duration_since(self.conversions[at].2) < RESAMPLE => at,
            found => {
                let conversion = ClockConversion::new(clock, target).ok_or_else(unrelated)?;
                match found {
                    Some(at) => {
                        self.conversions[at] = (clock, conversion, now);
                        at
                    }
                    None => {
                        self.conversions.push((clock, conversion, now));
                        self.conversions.len() - 1
                    }
                }
            }
        };
        Ok(self.conversions[at].1.apply(meta.timestamp))
    }
}
