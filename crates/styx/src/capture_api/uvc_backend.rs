//! The userspace UVC backend (`BackendKind::Uvc`, feature `uvc`): USB cameras driven over
//! usbfs by `styx-uvc`, without `uvcvideo`.
//!
//! Probing lists every UVC camera on the bus, also those `uvcvideo` holds; such a camera's
//! backend is merged into its V4L2 device *after* the V4L2 backend, so `uvcvideo` stays the
//! default and the planner only plans the userspace backend when asked for it
//! (`FrameRequirements` backend override, `CaptureRequest::backend`). Opening a camera
//! `uvcvideo` holds needs `UvcConfig::detach_kernel_driver` (or `STYX_UVC_DETACH=1`); the
//! driver gets it back when the capture ends. Frames are assembled straight into pooled
//! buffers and handed out without another copy; their timestamp is the capture time from the
//! camera's PTS when it sends PTS and SCR. Controls use the V4L2 ids and units `uvcvideo` uses.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, mpsc};
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use smallvec::{SmallVec, smallvec};
use styx_capture::prelude::*;
use styx_core::prelude::{
    BackendFrameMeta, ExternalBacking, FrameResidency, ResidencyTransition,
    ResidencyTransitionReason, TimestampClock, UvcFrameMeta,
};
use styx_uvc::controls::Kind;
use styx_uvc::{OpenOptions, UsbCameraInfo, UvcDevice, UvcError, UvcFrame};

use super::control_plane::ControlPlane;
use super::handle::{CaptureHandle, CaptureQueue, WorkerHandle};
use super::handle_metrics::deliver;
use super::request::CaptureError;
use super::tunables::StyxConfig;
use crate::metrics::StageMetrics;
use crate::{BackendHandle, BackendKind, DeviceIdentity, ProbedBackend, ProbedDevice};

fn uvc_err(e: UvcError) -> CaptureError {
    match e {
        UvcError::Disconnected => CaptureError::Disconnected("uvc camera disconnected".into()),
        UvcError::Invalid(m) => CaptureError::InvalidConfig(m),
        e => CaptureError::Backend(format!("uvc: {e}")),
    }
}

/// A UVC interval (100 ns units) as a Styx interval, reduced as `uvcvideo` reports it.
fn interval(v: u32) -> Option<Interval> {
    let (num, den) = styx_uvc::interval_fraction(v);
    Interval::new(num, den)
}

/// A Styx interval in 100 ns units.
fn interval_100ns(i: Interval) -> u32 {
    (u64::from(i.numerator.get()) * 10_000_000 / u64::from(i.denominator.get().max(1))) as u32
}

fn color(f: &styx_uvc::Format, code: FourCc) -> ColorSpace {
    match f.color.map(|c| c.matrix) {
        Some(1) => ColorSpace::Bt709,
        Some(4 | 5) if !code.is_compressed() => ColorSpace::Bt709,
        _ => code
            .info()
            .default_color_space
            .unwrap_or(ColorSpace::Unknown),
    }
}

/// One mode per format and frame size, with the frame's intervals.
pub(crate) fn modes(info: &UsbCameraInfo) -> Vec<Mode> {
    let mut modes = Vec::new();
    for vs in &info.function.streaming {
        for f in &vs.formats {
            let Some(cc) = f.fourcc() else { continue };
            let code = FourCc::new(cc);
            for fr in &f.frames {
                let Some(res) = Resolution::new(fr.width.into(), fr.height.into()) else {
                    continue;
                };
                let format = MediaFormat::new(code, res, color(f, code));
                let intervals: SmallVec<[Interval; 4]> = fr
                    .intervals
                    .listed(fr.default_interval)
                    .into_iter()
                    .filter_map(interval)
                    .collect();
                let interval_stepwise = match fr.intervals {
                    styx_uvc::Intervals::Continuous { min, max, step } => Some(IntervalStepwise {
                        min: interval(min).unwrap_or(intervals[0]),
                        max: interval(max).unwrap_or(intervals[0]),
                        step: Interval::new(step.max(1), 10_000_000).unwrap_or(intervals[0]),
                    }),
                    styx_uvc::Intervals::Discrete(_) => None,
                };
                if intervals.is_empty() {
                    continue;
                }
                modes.push(Mode {
                    id: ModeId {
                        format,
                        interval: None,
                    },
                    format,
                    intervals,
                    interval_stepwise,
                });
            }
        }
    }
    modes
}

/// Styx control descriptions of an opened camera's controls.
fn control_metas(dev: &UvcDevice) -> Vec<ControlMeta> {
    dev.controls()
        .into_iter()
        .map(|c| {
            let (kind, value): (ControlKind, fn(i64) -> ControlValue) = match c.def.kind {
                Kind::Bool => (ControlKind::Bool, |v| ControlValue::Bool(v != 0)),
                Kind::Menu | Kind::AeMode => (ControlKind::Menu, |v| ControlValue::Uint(v as u32)),
                Kind::Signed | Kind::Unsigned => {
                    (ControlKind::Int, |v| ControlValue::Int(v as i32))
                }
            };
            let menu = match c.def.kind {
                Kind::AeMode => Some(
                    [
                        "Auto Mode",
                        "Manual Mode",
                        "Shutter Priority Mode",
                        "Aperture Priority Mode",
                    ]
                    .map(String::from)
                    .to_vec(),
                ),
                Kind::Menu => Some(
                    ["Disabled", "50 Hz", "60 Hz", "Auto"]
                        .map(String::from)
                        .to_vec(),
                ),
                _ => None,
            };
            ControlMeta {
                id: ControlId(c.def.id),
                name: c.def.name.into(),
                kind,
                access: if c.info.set() {
                    Access::ReadWrite
                } else {
                    Access::ReadOnly
                },
                min: value(c.min),
                max: value(c.max),
                default: value(c.default),
                step: Some(ControlValue::Uint(c.step.max(1) as u32)),
                menu,
                metadata: ControlMetadata::default(),
            }
        })
        .collect()
}

/// Controls read once per plugged-in camera (port and device number), while nobody held it.
fn cached_controls(info: &UsbCameraInfo) -> Vec<ControlMeta> {
    type Cache = Mutex<HashMap<(String, u32), Vec<ControlMeta>>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = (info.key(), info.devnum);
    if let Some(c) = cache.lock().get(&key) {
        return c.clone();
    }
    if info.interfaces.iter().any(|i| i.driver.is_some()) {
        return Vec::new();
    }
    match UvcDevice::open(info.clone(), OpenOptions::default()) {
        Ok(dev) => {
            let metas = control_metas(&dev);
            cache.lock().insert(key, metas.clone());
            metas
        }
        Err(e) => {
            crate::trace::debug!(camera = %info.key(), error = %e, "uvc probe: controls not read");
            Vec::new()
        }
    }
}

fn properties(info: &UsbCameraInfo) -> Vec<(String, String)> {
    let vs = info.function.streaming.first();
    let mut p = vec![
        ("bus".into(), info.bus_info.clone()),
        ("usb_port".into(), info.port.clone()),
        ("usb_vendor".into(), format!("{:04x}", info.vendor_id)),
        ("usb_product".into(), format!("{:04x}", info.product_id)),
        ("speed".into(), format!("{:?}", info.speed)),
        (
            "uvc_version".into(),
            format!("{:x}", info.function.control.uvc_version),
        ),
        (
            "transfer".into(),
            format!("{:?}", vs.map(|v| v.transfer())).to_lowercase(),
        ),
        (
            "kernel_driver".into(),
            info.kernel_driver().unwrap_or("none").into(),
        ),
    ];
    if let Some(s) = &info.serial {
        p.push(("serial".into(), s.clone()));
    }
    p
}

/// The backend of one camera with `controls`.
fn backend_for(info: &UsbCameraInfo, controls: Vec<ControlMeta>) -> ProbedBackend {
    ProbedBackend {
        kind: BackendKind::Uvc,
        handle: BackendHandle::Uvc { key: info.key() },
        descriptor: CaptureDescriptor::new(modes(info)).with_controls(controls),
        properties: properties(info),
    }
}

/// Adds a camera's backend to the V4L2 device of the same camera (same bus id), after its
/// backends, or as a device of its own.
fn merge(devices: &mut Vec<ProbedDevice>, info: &UsbCameraInfo, backend: ProbedBackend) {
    let key = info.key();
    match devices
        .iter_mut()
        .find(|d| d.identity.keys.contains(&info.bus_info))
    {
        Some(d) => {
            d.backends.push(backend);
            if !d.identity.keys.contains(&key) {
                d.identity.keys.push(key);
            }
        }
        None => devices.push(ProbedDevice {
            identity: DeviceIdentity {
                display: info.name(),
                keys: vec![info.bus_info.clone(), key],
            },
            backends: vec![backend],
        }),
    }
}

/// Adds the UVC cameras on the bus to `devices` (see [`merge`]) when `backends` includes
/// this one.
pub(crate) fn probe_into(
    backends: Option<&[BackendKind]>,
    devices: &mut Vec<ProbedDevice>,
    errors: &mut Vec<crate::BackendProbeError>,
) {
    if !backends.is_none_or(|b| b.contains(&BackendKind::Uvc)) {
        return;
    }
    let (cams, errs) = styx_uvc::enumerate();
    errors.extend(
        errs.into_iter()
            .map(|e| crate::BackendProbeError::new(BackendKind::Uvc, e)),
    );
    for info in cams {
        let backend = backend_for(&info, cached_controls(&info));
        merge(devices, &info, backend);
    }
}

/// A frame's pooled buffer as a frame backing (no copy).
struct UvcBacking {
    frame: UvcFrame,
}

impl ExternalBacking for UvcBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        (index <= 2).then_some(&self.frame.data[..])
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.frame.data.capacity())
    }

    fn backing_kind(&self) -> &'static str {
        "uvc_usbfs"
    }

    fn residency(&self) -> FrameResidency {
        FrameResidency::HostExternal
    }
}

/// Plane layouts of a frame of `len` bytes.
fn layouts(code: FourCc, w: usize, h: usize, len: usize) -> Option<SmallVec<[PlaneLayout; 3]>> {
    let plane = |offset, len, stride| PlaneLayout {
        offset,
        len,
        stride,
    };
    if code.is_compressed() {
        return Some(smallvec![plane(0, len, len.max(1))]);
    }
    let layouts: SmallVec<[PlaneLayout; 3]> = match &code.to_u32().to_le_bytes() {
        b"NV12" | b"NV21" => smallvec![plane(0, w * h, w), plane(w * h, w * h / 2, w)],
        b"YU12" | b"YV12" => smallvec![
            plane(0, w * h, w),
            plane(w * h, w * h / 4, w / 2),
            plane(w * h * 5 / 4, w * h / 4, w / 2)
        ],
        b"GREY" => smallvec![plane(0, w * h, w)],
        b"YUYV" | b"UYVY" | b"YVYU" | b"VYUY" | b"RGBP" | b"Y16 " => {
            smallvec![plane(0, w * h * 2, w * 2)]
        }
        _ => smallvec![plane(0, len, len / h.max(1))],
    };
    let end = layouts.last().map_or(0, |p| p.offset + p.len);
    (end <= len).then_some(layouts)
}

fn frame_lease(frame: UvcFrame, clock_hz: u32, config: &StyxConfig) -> Option<FrameLease> {
    let code = FourCc::new(frame.fourcc);
    let res = Resolution::new(frame.width, frame.height)?;
    let format = MediaFormat::new(code, res, ColorSpace::Unknown);
    let planes = layouts(
        code,
        frame.width as usize,
        frame.height as usize,
        frame.data.len(),
    )?;
    let uvc = UvcFrameMeta {
        sequence: frame.sequence,
        bytes_used: frame.data.len() as u32,
        error: frame.flags.damaged(),
        no_eof: frame.flags.no_eof,
        timestamp_from_pts: frame.timestamp_from_pts,
        pts: frame.pts,
        scr_stc: frame.scr.map(|s| s.stc),
        scr_sof: frame.scr.map(|s| s.sof),
        clock_hz,
        first_payload_ns: frame.first_payload.as_nanos() as u64,
        last_payload_ns: frame.last_payload.as_nanos() as u64,
    };
    let clock = TimestampClock::Monotonic;
    let target = config.capture_tunables().timestamp_clock;
    // The payloads were copied into the frame as they arrived (the one copy of this path).
    styx_core::metrics::copied(styx_core::metrics::CopySite::Capture, frame.data.len());
    let mut meta = FrameMeta::new(format, frame.timestamp.as_nanos() as u64)
        .with_capture_instant(frame.dequeued)
        .with_sensor_latency(clock)
        .in_clock(clock, target.conversion_from(clock))
        .with_transition(ResidencyTransition {
            from: FrameResidency::HostExternal,
            to: FrameResidency::HostExternal,
            reason: ResidencyTransitionReason::Capture,
            copied: false,
        })
        .with_backend(BackendFrameMeta::Uvc(uvc));
    meta.hops.copied(frame.data.len());
    Some(FrameLease::from_external(
        meta,
        planes,
        Arc::new(UvcBacking { frame }),
    ))
}

/// Applies a control (V4L2 id and value, as `uvcvideo`).
pub(crate) fn apply_control(
    dev: &UvcDevice,
    id: ControlId,
    value: &ControlValue,
) -> Result<(), CaptureError> {
    let v = match value {
        ControlValue::Bool(v) => i64::from(*v),
        ControlValue::Int(v) => i64::from(*v),
        ControlValue::Uint(v) => i64::from(*v),
        ControlValue::Float(v) => v.round() as i64,
        _ => return Err(CaptureError::control_apply("uvc controls take numbers")),
    };
    dev.set_control(id.0, v).map_err(|e| match e {
        UvcError::Unsupported(_) => CaptureError::ControlUnsupported,
        e => CaptureError::control_apply(e.to_string()),
    })
}

/// Reads a control's current value.
pub(crate) fn read_control(dev: &UvcDevice, id: ControlId) -> Result<ControlValue, CaptureError> {
    let kind = styx_uvc::controls::find(id.0).map(|d| d.kind);
    let v = dev.control(id.0).map_err(|e| match e {
        UvcError::Unsupported(_) => CaptureError::ControlUnsupported,
        e => CaptureError::control_apply(e.to_string()),
    })?;
    Ok(match kind {
        Some(Kind::Bool) => ControlValue::Bool(v != 0),
        Some(Kind::Menu | Kind::AeMode) => ControlValue::Uint(v as u32),
        _ => ControlValue::Int(v as i32),
    })
}

pub(super) fn start_uvc(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    initial: Vec<(ControlId, ControlValue)>,
    mut descriptor: CaptureDescriptor,
    config: &StyxConfig,
    queue: Option<CaptureQueue>,
) -> Result<CaptureHandle, CaptureError> {
    let BackendHandle::Uvc { key } = &backend.handle else {
        return Err(CaptureError::BackendUnavailable(BackendKind::Uvc));
    };
    let uvc = config.uvc_config();
    let interval = interval.or_else(|| crate::planner::default_interval(&mode));
    let device = UvcDevice::open_key(
        key,
        OpenOptions {
            detach_kernel_driver: uvc.detach(),
        },
    )
    .map_err(uvc_err)?;
    let res = mode.format.resolution;
    let mut stream_config = device
        .find_mode(
            mode.format.code.to_u32().to_le_bytes(),
            res.width.get(),
            res.height.get(),
            interval.map(interval_100ns),
        )
        .ok_or_else(|| {
            CaptureError::InvalidConfig(format!(
                "uvc: no mode {} {}x{}",
                mode.format.code, res.width, res.height
            ))
        })?;
    stream_config.urbs = uvc.urbs;
    stream_config.packets_per_urb = uvc.packets_per_urb;
    stream_config.deliver_damaged = uvc.deliver_damaged;
    stream_config.buffers = config.capture_tunables().queue_depth.max(2) + 2;
    if descriptor.controls.is_empty() {
        descriptor.controls = control_metas(&device);
    }
    for (id, value) in &initial {
        apply_control(&device, *id, value)?;
    }
    let mut stream = device.start(stream_config).map_err(uvc_err)?;
    let clock_hz = stream
        .format()
        .params
        .clock_frequency
        .max(device.function().control.clock_frequency);

    let capture = config.capture_tunables();
    // A supervised capture hands in the consumer's queue: it outlives this capture (a
    // restart sends into it), so only a queue of our own is closed when the worker ends.
    let owns_queue = queue.is_none();
    let (tx, rx) = queue.unwrap_or_else(|| {
        styx_core::queue::bounded_with(capture.queue_depth.max(1), capture.queue_overflow)
    });
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let worker_error = Arc::new(Mutex::new(None));
    let worker_error_for_thread = worker_error.clone();
    let send_timeout = Duration::from_millis(capture.queue_send_timeout_ms);
    let poll = Duration::from_millis(capture.idle_poll_ms.clamp(5, 100));
    let config_for_thread = config.clone();
    // Gaps in UVC sequence numbers are damaged frames styx-uvc dropped.
    let live = crate::metrics::CaptureMetrics::default().gaps_are_corrupt();
    let live_worker = live.clone();
    let worker = thread::Builder::new()
        .name("styx-uvc-capture".into())
        .spawn(move || {
            live_worker.register_thread();
            crate::trace::debug!(backend = "uvc", "capture worker started");
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                match stream.next_blocking(poll) {
                    Ok(frame) => {
                        let Some(lease) = frame_lease(frame, clock_hz, &config_for_thread) else {
                            continue;
                        };
                        if deliver(&live_worker, &tx, lease, "uvc", send_timeout) {
                            break;
                        }
                    }
                    Err(UvcError::Timeout) => {}
                    Err(e) => {
                        crate::trace::warn!(backend = "uvc", error = %e, "capture failed");
                        *worker_error_for_thread.lock() = Some(uvc_err(e));
                        break;
                    }
                }
            }
            let stats = stream.stats();
            drop(stream);
            if owns_queue {
                tx.close();
            }
            crate::trace::debug!(backend = "uvc", ?stats, "capture worker stopped");
        })
        .map_err(|e| CaptureError::Backend(format!("uvc worker: {e}")))?;
    Ok(CaptureHandle {
        backend: BackendKind::Uvc,
        control: ControlPlane::Uvc { device },
        descriptor,
        mode,
        interval,
        rx,
        stop_tx: Some(stop_tx),
        worker: Some(WorkerHandle::Thread(worker)),
        aux_workers: Vec::new(),
        #[cfg(feature = "libcamera")]
        libcamera_idle_stop_allowed: false,
        #[cfg(feature = "libcamera")]
        libcamera_stop_when_idle: false,
        metrics: StageMetrics::default(),
        external_backings: Vec::new(),
        worker_error,
        control_error: Arc::new(Mutex::new(None)),
        shutdown_stats: Default::default(),
        retry_metrics: Default::default(),
        sequence_gaps: Default::default(),
        live,
    })
}

#[cfg(test)]
#[path = "uvc_backend_tests.rs"]
mod tests;
