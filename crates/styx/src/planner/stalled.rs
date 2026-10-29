//! Modes that started but delivered no frame. An uncompressed mode of a USB camera can take
//! more of the bus than a hub shared with other devices leaves it: the camera streams, and no
//! frame ever arrives (a Logitech C270's YUYV 640x480 behind a shared hub). Starting such a mode
//! waits for its first frame; when none comes, the mode is remembered for the camera with the
//! cause, the planner leaves it out from then on (its rejection says why), and
//! [`FrameRequest::open`](super::FrameRequest::open) and the camera service plan again, which
//! picks the next mode: on a USB 2 camera the compressed modes come first (MJPG, see
//! `planner::shared_bus_uncompressed`), then the smaller uncompressed ones. The fallback keeps
//! planning until a mode streams or none is left; a refused or stalled mode is never started
//! again while the record lasts.

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

/// How long a mode the bus refused stays out of the plans. Bandwidth frees up when another
/// camera stops, so the mode is tried again after this.
const BUS_FULL_FOR: Duration = Duration::from_secs(10);

/// Most modes a fallback starts before it gives up (each refusal or dead mode is left out).
const MAX_FALLBACKS: usize = 16;

struct BusFull {
    keys: Vec<String>,
    backend: BackendKind,
    mode: ModeId,
    /// The bytes per second the refused mode carries, when it is uncompressed: every uncompressed
    /// mode at least as heavy is refused too, without another start (see [`check`]).
    rate: Option<f64>,
    until: Instant,
    cause: String,
}

static BUS_FULL: Mutex<Vec<BusFull>> = Mutex::new(Vec::new());

/// Whether starting `mode` on `backend` waits for its first frame: uncompressed modes of USB
/// cameras.
pub(crate) fn may_starve(backend: &ProbedBackend, mode: &Mode) -> bool {
    !mode.format.code.is_compressed() && super::usb_bus_mbps(backend).is_some()
}

/// `Err(reason)` when `mode` started on `device` before and delivered no frame, or the bus
/// refused it lately: the same mode, or an uncompressed mode carrying at least as many bytes per
/// second as one the bus refused. Such a mode would be refused as well, and each refused start
/// costs the driver a burst of messages, so it is not tried.
pub(crate) fn check(
    device: &ProbedDevice,
    backend: BackendKind,
    mode: &Mode,
) -> Result<(), String> {
    let stalled = STALLED.lock();
    if let Some(s) = stalled
        .iter()
        .find(|s| s.backend == backend && s.mode == mode.id && shares_key(&s.keys, device))
    {
        return Err(format!("delivered no frames when started: {}", s.cause));
    }
    drop(stalled);
    let now = Instant::now();
    let rate = super::uncompressed_bytes_per_second(mode);
    let full = BUS_FULL.lock();
    match full.iter().find(|b| {
        b.until > now
            && b.backend == backend
            && shares_key(&b.keys, device)
            && (b.mode == mode.id || matches!((b.rate, rate), (Some(r), Some(m)) if m >= r))
    }) {
        Some(b) => Err(format!("not enough USB bandwidth on this bus; {}", b.cause)),
        None => Ok(()),
    }
}

/// Whether a backend's error text is the driver refusing a stream for lack of bus bandwidth
/// (ENOSPC from `VIDIOC_STREAMON` or an alternate-setting selection).
fn is_bus_full(text: &str) -> bool {
    text.contains("No space left on device") || text.contains("os error 28")
}

fn is_usb(device: &ProbedDevice, backend: BackendKind) -> bool {
    device
        .backends
        .iter()
        .find(|b| b.kind == backend)
        .and_then(super::usb_bus_mbps)
        .is_some()
}

/// A start failure as the caller sees it: on a USB camera the bus refusing the stream becomes
/// [`CaptureError::NotEnoughBandwidth`], and the mode is kept out of the plans for a while
/// (logged once when it is first refused). Other errors are returned as they are.
pub(crate) fn start_failed(
    device: &ProbedDevice,
    backend: BackendKind,
    mode: &Mode,
    err: CaptureError,
) -> CaptureError {
    let CaptureError::Backend(text) = &err else {
        return err;
    };
    if !is_bus_full(text) || !is_usb(device, backend) {
        return err;
    }
    let cause = format!(
        "{:?} does not fit the bus now (the driver refused it: {text}); lower modes are \
         tried, and this one again in {} s",
        mode.id,
        BUS_FULL_FOR.as_secs()
    );
    let now = Instant::now();
    let mut full = BUS_FULL.lock();
    full.retain(|b| b.until > now);
    let fresh = !full
        .iter()
        .any(|b| b.backend == backend && b.mode == mode.id && shares_key(&b.keys, device));
    if fresh {
        full.push(BusFull {
            keys: device.identity.keys.clone(),
            backend,
            mode: mode.id.clone(),
            rate: super::uncompressed_bytes_per_second(mode),
            until: now + BUS_FULL_FOR,
            cause: cause.clone(),
        });
        crate::trace::warn!(camera = %device.identity.display, %cause, "not enough USB bandwidth on this bus");
    }
    CaptureError::NotEnoughBandwidth(cause)
}

/// Plan and start again while a mode is refused or delivers no frame: each such mode is left
/// out, so the next plan is the next mode. The planner ranks the camera's compressed modes first
/// (see `planner::shared_bus_uncompressed`), so a fallback tries MJPG before smaller uncompressed
/// modes. Stops at the first mode that streams; after [`MAX_FALLBACKS`] modes, or when no mode
/// is left to plan, the last error is returned, since bandwidth may free up.
pub(crate) fn open_with_fallback<P, T>(
    plan: impl Fn() -> Result<P, super::PlanError>,
    start: impl Fn(&P) -> Result<T, CaptureError>,
) -> Result<T, super::OpenError> {
    let mut last: Option<CaptureError> = None;
    for _ in 0..MAX_FALLBACKS {
        let planned = match plan() {
            Ok(planned) => planned,
            Err(err) => {
                return Err(match last {
                    Some(last) => super::OpenError::Capture(last),
                    None => super::OpenError::Plan(err),
                });
            }
        };
        match start(&planned) {
            Ok(started) => return Ok(started),
            Err(err @ (CaptureError::NotEnoughBandwidth(_) | CaptureError::NoFrames(_))) => {
                last = Some(err);
            }
            Err(err) => return Err(err.into()),
        }
    }
    Err(last
        .unwrap_or_else(|| CaptureError::NoFrames("no mode delivered frames".into()))
        .into())
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
                return Err(start_failed(device, backend, mode, err));
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

/// What a start of `mode` on `device` does instead of the driver, in tests (see
/// [`script`]); `None` runs the real start. Production always runs it.
#[cfg(not(test))]
#[inline]
pub(crate) fn start_hook(
    _device: &ProbedDevice,
    _backend: BackendKind,
    _mode: &Mode,
) -> Option<Result<CaptureHandle, CaptureError>> {
    None
}

/// The start seam, for tests: a USB camera's start is refused, stalled, delayed, or streams from
/// a virtual device (the driver is not there), as a script says. Every start of one is logged.
#[cfg(test)]
pub(crate) fn start_hook(
    device: &ProbedDevice,
    backend: BackendKind,
    mode: &Mode,
) -> Option<Result<CaptureHandle, CaptureError>> {
    use crate::capture_api::CaptureRequest;
    use crate::capture_api::make_virtual_device;
    if !is_usb(device, backend) {
        return None;
    }
    let camera = device.identity.display.clone();
    let (code, width) = (mode.format.code, mode.format.resolution.width.get());
    script::STARTS.lock().push((camera.clone(), code, width));
    let delay = script::DELAY
        .lock()
        .iter()
        .find(|(name, _)| *name == camera)
        .map(|(_, d)| *d);
    if let Some(delay) = delay {
        std::thread::sleep(delay);
    }
    let outcome = script::RULES
        .lock()
        .iter()
        .find(|r| r.camera == camera && r.code == code && r.width == width)
        .map(|r| r.outcome);
    match outcome {
        Some(script::Outcome::Refuse) => Some(Err(CaptureError::Backend(
            "uvc: No space left on device (os error 28)".into(),
        ))),
        Some(script::Outcome::Stall) => {
            let cause = format!("{code} {width}: no frame (scripted)");
            record(device, backend, &mode.id, &cause);
            Some(Err(CaptureError::NoFrames(cause)))
        }
        None => {
            let virtual_device = make_virtual_device(&camera, [mode.clone()]);
            Some(
                CaptureRequest::new(&virtual_device)
                    .mode(mode.id.clone())
                    .start(),
            )
        }
    }
}

#[cfg(test)]
pub(crate) fn mark(device: &ProbedDevice, backend: BackendKind, mode: &ModeId, cause: &str) {
    record(device, backend, mode, cause);
}

/// The scripted starts of [`start_hook`] (tests only). Cameras are told apart by display name,
/// so tests running in parallel each use their own.
#[cfg(test)]
#[cfg_attr(
    not(all(feature = "codec-turbojpeg", feature = "raw-decoders")),
    allow(dead_code)
)]
pub(crate) mod script {
    use parking_lot::Mutex;
    use std::time::Duration;

    use styx_core::prelude::FourCc;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Outcome {
        /// The bus refuses the stream (ENOSPC).
        Refuse,
        /// The stream starts and delivers no frame.
        Stall,
    }

    pub(crate) struct Rule {
        pub(crate) camera: String,
        pub(crate) code: FourCc,
        pub(crate) width: u32,
        pub(crate) outcome: Outcome,
    }

    pub(crate) static RULES: Mutex<Vec<Rule>> = Mutex::new(Vec::new());
    /// Every start: camera, format and width, in order.
    pub(crate) static STARTS: Mutex<Vec<(String, FourCc, u32)>> = Mutex::new(Vec::new());
    /// A delay before each start of the camera.
    pub(crate) static DELAY: Mutex<Vec<(String, Duration)>> = Mutex::new(Vec::new());

    /// The starts of `camera` so far, as `(format, width)`.
    pub(crate) fn starts(camera: &str) -> Vec<(FourCc, u32)> {
        STARTS
            .lock()
            .iter()
            .filter(|(name, _, _)| name == camera)
            .map(|(_, code, width)| (*code, *width))
            .collect()
    }

    pub(crate) fn refuse(camera: &str, code: FourCc, width: u32) {
        rule(camera, code, width, Outcome::Refuse);
    }

    pub(crate) fn stall(camera: &str, code: FourCc, width: u32) {
        rule(camera, code, width, Outcome::Stall);
    }

    fn rule(camera: &str, code: FourCc, width: u32, outcome: Outcome) {
        RULES.lock().push(Rule {
            camera: camera.to_string(),
            code,
            width,
            outcome,
        });
    }

    pub(crate) fn delay(camera: &str, by: Duration) {
        DELAY.lock().push((camera.to_string(), by));
    }
}
