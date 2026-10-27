//! Modes that started but delivered no frame. An uncompressed mode of a USB camera can take
//! more of the bus than a hub shared with other devices leaves it: the camera streams, and no
//! frame ever arrives (a Logitech C270's YUYV 640x480 behind a shared hub). Starting such a mode
//! waits for its first frame; when none comes, the mode is remembered for the camera with the
//! cause, the planner leaves it out from then on (its rejection says why), and
//! [`FrameRequest::open`](super::FrameRequest::open) and the camera service plan again, which
//! picks the next mode (MJPG of the same size).

use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_capture::prelude::{Mode, ModeId};
use styx_core::prelude::*;

use crate::capture_api::{CaptureError, CaptureHandle};
use crate::{BackendKind, ProbedBackend, ProbedDevice};

/// How long a mode that may starve gets for its first frame, at least (four frame intervals
/// when that is longer). USB cameras deliver their first frame within about half a second.
const FIRST_FRAME: Duration = Duration::from_secs(2);

struct Stalled {
    keys: Vec<String>,
    backend: BackendKind,
    mode: ModeId,
    cause: String,
}

static STALLED: Mutex<Vec<Stalled>> = Mutex::new(Vec::new());

/// Whether starting `mode` on `backend` waits for its first frame: uncompressed modes of USB
/// cameras.
pub(crate) fn may_starve(backend: &ProbedBackend, mode: &Mode) -> bool {
    !mode.format.code.is_compressed() && super::usb_bus_mbps(backend).is_some()
}

/// `Err(reason)` when `mode` started on `device` before and delivered no frame.
pub(crate) fn check(
    device: &ProbedDevice,
    backend: BackendKind,
    mode: &ModeId,
) -> Result<(), String> {
    let stalled = STALLED.lock();
    match stalled
        .iter()
        .find(|s| s.backend == backend && s.mode == *mode && shares_key(&s.keys, device))
    {
        Some(s) => Err(format!("delivered no frames when started: {}", s.cause)),
        None => Ok(()),
    }
}

fn shares_key(keys: &[String], device: &ProbedDevice) -> bool {
    device.identity.keys.iter().any(|k| keys.contains(k))
}

fn record(device: &ProbedDevice, backend: BackendKind, mode: &ModeId, cause: &str) {
    let mut stalled = STALLED.lock();
    if !stalled
        .iter()
        .any(|s| s.backend == backend && s.mode == *mode && shares_key(&s.keys, device))
    {
        stalled.push(Stalled {
            keys: device.identity.keys.clone(),
            backend,
            mode: mode.clone(),
            cause: cause.to_string(),
        });
    }
}

/// Wait for `capture`'s first frame when the mode may starve ([`may_starve`]). Without one in
/// time the mode is remembered as stalled and the error says why; the caller drops the capture.
pub(crate) fn confirm_first_frame(
    capture: &CaptureHandle,
    device: &ProbedDevice,
    mode: &Mode,
    interval: Option<Interval>,
) -> Result<(), CaptureError> {
    let backend = capture.backend();
    let Some(probed) = device.backends.iter().find(|b| b.kind == backend) else {
        return Ok(());
    };
    if !may_starve(probed, mode) {
        return Ok(());
    }
    let frame = interval.map_or(Duration::ZERO, |i| {
        Duration::from_secs_f32(1.0 / i.fps().max(0.01))
    });
    let wait = FIRST_FRAME.max(frame * 4);
    let start = Instant::now();
    while start.elapsed() < wait {
        if capture.queue_stats().sent > 0 {
            return Ok(());
        }
        if let Some(err) = capture.last_error() {
            // A frame could still have arrived just before the error.
            if capture.queue_stats().sent > 0 {
                return Ok(());
            }
            if !matches!(err, CaptureError::Disconnected(_)) {
                return Err(err);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mbps = super::usb_bus_mbps(probed).unwrap_or(480.0);
    let res = mode.format.resolution;
    let cause = format!(
        "{} {}x{} on a {mbps:.0} Mbit/s USB bus: no frame in {} ms; uncompressed USB modes \
         starve when a hub or another device shares the bus",
        mode.format.code,
        res.width,
        res.height,
        wait.as_millis()
    );
    record(device, backend, &mode.id, &cause);
    crate::trace::warn!(camera = %device.identity.display, %cause, "mode delivered no frames; it is left out from now on");
    Err(CaptureError::NoFrames(cause))
}

#[cfg(test)]
pub(crate) fn mark(device: &ProbedDevice, backend: BackendKind, mode: &ModeId, cause: &str) {
    record(device, backend, mode, cause);
}
