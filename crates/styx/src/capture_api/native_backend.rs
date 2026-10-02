//! The native backend: sensors Styx drives itself through the Styx sensor bridge
//! (`styx-native`). Probing lists each bridged camera with its modes (every rate the sensor
//! timing allows, exactly); capture delivers raw frames as dma-buf backed [`FrameLease`]s that
//! carry the exposure, gain and frame duration that produced them; controls go through the
//! frame-accurate control schedule.

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use smallvec::smallvec;
use styx_capture::prelude::*;
use styx_core::prelude::{
    BackendFrameMeta, ExternalBacking, FrameBackingExport, FrameExportError, FrameFdPlane,
    FrameResidency, NativeFrameMeta, TimestampClock,
};
use styx_native::{
    CameraControls, CameraInfo, CameraOptions, NativeError, NativeFrame, SensorLibrary, SensorMode,
    StreamSettings,
};

use super::control_plane::ControlPlane;
use super::handle::{CaptureHandle, CaptureQueue, WorkerHandle, enqueue_capture_frame};
use super::request::CaptureError;
use super::tunables::StyxConfig;
use crate::metrics::StageMetrics;
use crate::{BackendHandle, BackendKind, DeviceIdentity, ProbedBackend, ProbedDevice};

/// Control ids of native cameras (the control plane's `ControlId`s).
pub mod controls {
    use styx_capture::prelude::ControlId;

    /// Exposure time in microseconds (`Uint`).
    pub const EXPOSURE_TIME_US: ControlId = ControlId(0xF400_0001);
    /// Total gain, analogue first then digital, as a ratio (`Float`).
    pub const GAIN: ControlId = ControlId(0xF400_0002);
    /// Frame duration in microseconds (`Uint`); sets the frame length exactly.
    pub const FRAME_DURATION_US: ControlId = ControlId(0xF400_0003);
    /// Frame rate in frames per second (`Float`); sets the closest frame length.
    pub const FRAME_RATE: ControlId = ControlId(0xF400_0004);
    /// AE state of a processed mode's 3A loop after the latest frame, as libcamera's
    /// `AeState` (`Int`, read only): 1 searching, 2 converged (AE locked).
    pub const AE_STATE: ControlId = ControlId(0xF400_0010);
}

fn native_err(e: NativeError) -> CaptureError {
    match e {
        e if e.is_disconnect() => CaptureError::Disconnected(format!("native camera: {e}")),
        NativeError::InvalidConfig(m) => CaptureError::InvalidConfig(m),
        NativeError::Busy(m) => CaptureError::Backend(format!("native: busy: {m}")),
        e => CaptureError::Backend(format!("native: {e}")),
    }
}

/// The capture mode of a sensor mode in a pixel format: the default rate first, the fastest
/// second, and every rate in between through the stepwise range.
fn capture_mode(m: &SensorMode, fourcc: u32) -> Option<Mode> {
    let res = Resolution::new(m.width, m.height)?;
    let format = MediaFormat::new(FourCc::from(fourcc), res, ColorSpace::Unknown);
    let iv = |f: styx_native::Fraction| Interval::new(f.num, f.den);
    let (default, fastest, slowest) = (
        iv(m.default_interval)?,
        iv(m.min_interval)?,
        iv(m.max_interval)?,
    );
    Some(Mode {
        id: ModeId {
            format,
            interval: None,
        },
        format,
        intervals: smallvec![default, fastest],
        // A step of 0.001 fps: any rate the timing allows.
        interval_stepwise: Some(IntervalStepwise {
            min: fastest,
            max: slowest,
            step: Interval::new(1000, 1)?,
        }),
    })
}

fn control_metas(info: &CameraInfo) -> Vec<ControlMeta> {
    let d = &info.description;
    let first = info.modes.first();
    let (exp_min, exp_max, exp_default) = first.map_or((1, 1_000_000, 10_000), |m| {
        let t = &m.timing;
        let l = t.exposure_limits(t.frame_length_max());
        let default = t.lines_to_duration(f64::from(d.controls.exposure.default));
        (
            l.min.as_micros().max(1) as u32,
            l.max.as_micros() as u32,
            default.as_micros() as u32,
        )
    });
    let g = &d.controls.analog_gain;
    let (mut gmin, mut gmax) = (g.gain_for_code(g.min_code), g.gain_for_code(g.max_code));
    if let Some(dg) = &d.controls.digital_gain {
        gmin *= dg.gain_for_code(dg.min_code);
        gmax *= dg.gain_for_code(dg.max_code);
    }
    let (fmin, fmax) = info.modes.iter().fold((f64::MAX, 0.0f64), |(lo, hi), m| {
        (lo.min(m.min_fps()), hi.max(m.max_fps()))
    });
    let us = |f: f64| (1e6 / f.max(1e-6)) as u32;
    let meta = |id, name: &str, kind, min, max, default, step| ControlMeta {
        id,
        name: name.into(),
        kind,
        access: Access::ReadWrite,
        min,
        max,
        default,
        step: Some(step),
        menu: None,
        metadata: ControlMetadata::default(),
    };
    vec![
        meta(
            controls::EXPOSURE_TIME_US,
            "exposure_time_us",
            ControlKind::Uint,
            ControlValue::Uint(exp_min),
            ControlValue::Uint(exp_max),
            ControlValue::Uint(exp_default),
            ControlValue::Uint(1),
        ),
        meta(
            controls::GAIN,
            "gain",
            ControlKind::Float,
            ControlValue::Float(gmin as f32),
            ControlValue::Float(gmax as f32),
            ControlValue::Float(gmin as f32),
            ControlValue::Float(0.0625),
        ),
        meta(
            controls::FRAME_DURATION_US,
            "frame_duration_us",
            ControlKind::Uint,
            ControlValue::Uint(us(fmax)),
            ControlValue::Uint(us(fmin)),
            ControlValue::Uint(first.map_or(33_333, |m| us(m.default_interval.fps()))),
            ControlValue::Uint(1),
        ),
        meta(
            controls::FRAME_RATE,
            "frame_rate",
            ControlKind::Float,
            ControlValue::Float(fmin as f32),
            ControlValue::Float(fmax as f32),
            ControlValue::Float(first.map_or(30.0, |m| m.default_interval.fps() as f32)),
            ControlValue::Float(0.001),
        ),
    ]
}

/// The probed device of a bridged camera: its raw modes, and `NV12` / `RG24` modes processed
/// by the PiSP or the software ISP with the 3A loop (property `isp`).
pub(crate) fn probed_device(info: &CameraInfo) -> ProbedDevice {
    let mut modes = Vec::new();
    for m in &info.modes {
        for f in styx_native::graph::raw_formats_for(m.code, &info.raw_formats) {
            modes.extend(capture_mode(m, f.0));
        }
    }
    let processed = super::native_isp::processed_modes(&modes);
    modes.extend(processed);
    let mut properties = info.properties();
    properties.push(("isp".into(), super::native_isp::isp_name(info).into()));
    let descriptor = CaptureDescriptor::new(modes).with_controls(control_metas(info));
    ProbedDevice {
        identity: DeviceIdentity {
            display: info.display_name(),
            keys: info.identity(),
        },
        backends: vec![ProbedBackend {
            kind: BackendKind::Native,
            handle: BackendHandle::Native {
                key: info.key.clone(),
            },
            descriptor,
            properties,
        }],
    }
}

/// Bridged cameras and the problems met while looking.
pub(crate) fn probe() -> (Vec<ProbedDevice>, Vec<String>) {
    let (cameras, errors) = styx_native::discover(&SensorLibrary::system());
    (
        cameras.iter().map(probed_device).collect(),
        errors.into_iter().map(|e| e.to_string()).collect(),
    )
}

/// A frame's buffer as a frame backing: mapped for reading, exportable as its dma-buf.
struct NativeBacking {
    frame: NativeFrame,
    len: usize,
}

impl ExternalBacking for NativeBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        (index == 0).then(|| {
            let d = self.frame.data();
            &d[..self.len.min(d.len())]
        })
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.frame.buffer_len())
    }

    fn backing_kind(&self) -> &'static str {
        "native_dmabuf"
    }

    fn can_export(&self) -> bool {
        self.frame.dmabuf().is_some()
    }

    fn residency(&self) -> FrameResidency {
        FrameResidency::Dmabuf
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        let Some(fd) = self.frame.dmabuf() else {
            return Ok(None);
        };
        let fd = fd.try_clone_to_owned().map_err(FrameExportError::Fd)?;
        Ok(Some(FrameBackingExport::DmabufPlanes {
            planes: vec![FrameFdPlane {
                fd,
                offset: 0,
                len: self.len,
            }],
        }))
    }
}

/// Wraps a native frame as a frame lease (no copy).
pub(crate) fn frame_lease(frame: NativeFrame) -> Option<FrameLease> {
    let res = Resolution::new(frame.width, frame.height)?;
    let format = MediaFormat::new(FourCc::from(frame.fourcc.0), res, ColorSpace::Unknown);
    let len = frame.stride as usize * frame.height as usize;
    let c = frame.controls;
    let native = NativeFrameMeta {
        sequence: frame.sequence,
        bytes_used: frame.bytes_used as u32,
        error: frame.error,
        exposure_ns: c.map_or(0, |c| c.exposure.as_nanos() as u64),
        analog_gain: c.map_or(1.0, |c| c.analog_gain as f32),
        digital_gain: c.map_or(1.0, |c| c.digital_gain as f32),
        frame_duration_ns: c.map_or(0, |c| c.frame_duration.as_nanos() as u64),
        frame_length: c.map_or(0, |c| c.frame_length),
        verified: c.is_some_and(|c| c.verified),
    };
    let mut meta = FrameMeta::new(format, frame.timestamp.as_nanos() as u64)
        .with_backend(BackendFrameMeta::Native(native))
        .with_capture_instant(frame.dequeued);
    meta.clock = Some(TimestampClock::Monotonic);
    let layout = PlaneLayout {
        offset: 0,
        len,
        stride: frame.stride as usize,
    };
    Some(FrameLease::from_external(
        meta,
        smallvec![layout],
        Arc::new(NativeBacking { frame, len }),
    ))
}

/// Applies one control through the native control schedule.
pub(crate) fn apply_control(
    controls: &CameraControls,
    id: ControlId,
    value: &ControlValue,
) -> Result<(), CaptureError> {
    let num = match value {
        ControlValue::Uint(v) => f64::from(*v),
        ControlValue::Int(v) => f64::from(*v),
        ControlValue::Float(v) => f64::from(*v),
        _ => return Err(CaptureError::control_apply("native controls take numbers")),
    };
    let r = match id {
        controls::EXPOSURE_TIME_US => controls.set_exposure(Duration::from_secs_f64(num / 1e6)),
        controls::GAIN => controls.set_gain(num),
        controls::FRAME_DURATION_US => {
            controls.set_frame_duration(Duration::from_secs_f64(num / 1e6))
        }
        controls::FRAME_RATE => controls.set_frame_rate(num),
        _ => return Err(CaptureError::ControlUnsupported),
    };
    r.map(|_| ())
        .map_err(|e| CaptureError::control_apply(e.to_string()))
}

/// Reads a control: the value in effect for the latest frame.
pub(crate) fn read_control(
    controls: &CameraControls,
    id: ControlId,
) -> Result<ControlValue, CaptureError> {
    let c = controls
        .current()
        .ok_or_else(|| CaptureError::control_apply("no mode configured"))?;
    Ok(match id {
        controls::EXPOSURE_TIME_US => ControlValue::Uint(c.exposure.as_micros() as u32),
        controls::GAIN => ControlValue::Float(c.gain() as f32),
        controls::FRAME_DURATION_US => ControlValue::Uint(c.frame_duration.as_micros() as u32),
        controls::FRAME_RATE => {
            ControlValue::Float((1.0 / c.frame_duration.as_secs_f64().max(1e-9)) as f32)
        }
        _ => return Err(CaptureError::ControlUnsupported),
    })
}

pub(super) fn start_native(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    initial: Vec<(ControlId, ControlValue)>,
    descriptor: CaptureDescriptor,
    config: &StyxConfig,
    queue: Option<CaptureQueue>,
) -> Result<CaptureHandle, CaptureError> {
    let BackendHandle::Native { key } = &backend.handle else {
        return Err(CaptureError::BackendUnavailable(BackendKind::Native));
    };
    let provider = styx_native::NativeProvider::new(SensorLibrary::system())
        .with_options(CameraOptions::default());
    if super::native_isp::is_processed(mode.format.code) {
        let camera = super::native_isp::open_for_isp(&provider, key, config)?;
        // Exposure and gain belong to the 3A loop; initial controls are not applied.
        return super::native_isp::start_processed(
            camera, mode, interval, descriptor, config, queue,
        );
    }
    let mut camera = provider.open_camera(key).map_err(native_err)?;
    let settings = StreamSettings {
        width: mode.format.resolution.width.get(),
        height: mode.format.resolution.height.get(),
        fourcc: Some(styx_kernel_fourcc(mode.format.code)),
        code: None,
        interval: interval
            .map(|i| styx_native::Fraction::new(i.numerator.get(), i.denominator.get())),
    };
    camera.configure(&settings).map_err(native_err)?;
    let controls = camera.controls();
    for (id, value) in &initial {
        apply_control(&controls, *id, value)?;
    }
    let mut stream = camera.start().map_err(native_err)?;

    let capture = config.capture_tunables();
    let (tx, rx) = queue.unwrap_or_else(|| {
        styx_core::queue::bounded_with(capture.queue_depth.max(1), capture.queue_overflow)
    });
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let worker_error = Arc::new(Mutex::new(None));
    let worker_error_for_thread = worker_error.clone();
    let send_timeout = Duration::from_millis(capture.queue_send_timeout_ms);
    let poll = Duration::from_millis(capture.idle_poll_ms.clamp(5, 100));
    let worker = thread::Builder::new()
        .name("styx-native-capture".into())
        .spawn(move || {
            tracing::debug!(backend = "native", "capture worker started");
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                match stream.next_blocking(poll) {
                    Ok(Some(frame)) => {
                        let Some(lease) = frame_lease(frame) else {
                            continue;
                        };
                        if enqueue_capture_frame(&tx, lease, "native", send_timeout) {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(NativeError::Timeout) => {}
                    Err(e) => {
                        tracing::warn!(backend = "native", error = %e, "capture failed");
                        *worker_error_for_thread.lock() = Some(native_err(e));
                        break;
                    }
                }
            }
            drop(stream);
            if let Err(e) = camera.close() {
                tracing::warn!(backend = "native", error = %e, "closing the camera");
            }
            tx.close();
            tracing::debug!(backend = "native", "capture worker stopped");
        })
        .map_err(|e| CaptureError::Backend(format!("native worker: {e}")))?;
    Ok(CaptureHandle {
        backend: BackendKind::Native,
        control: ControlPlane::Native {
            controls,
            ae_state: None,
        },
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
    })
}

fn styx_kernel_fourcc(code: FourCc) -> styx_native::KernelFourCc {
    styx_native::KernelFourCc(code.to_u32())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ov9782() -> Vec<SensorMode> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../sensor/sensors/ov9782.toml");
        let desc = SensorLibrary::new().with_path(path).find("ov9782").unwrap();
        styx_native::modes::sensor_modes(&desc)
    }

    #[test]
    fn modes_cover_every_rate_exactly() {
        let modes = ov9782();
        let mode = capture_mode(&modes[0], u32::from_le_bytes(*b"pBAA")).unwrap();
        assert_eq!(mode.format.code.to_string(), "pBAA");
        assert_eq!(mode.format.resolution.width.get(), 1280);
        let sw = mode.interval_stepwise.unwrap();
        for fps in [3, 30, 60, 90, 120] {
            assert!(sw.contains(Interval::from_fps(fps).unwrap()), "{fps}");
        }
        assert!(!sw.contains(Interval::from_fps(121).unwrap()));
        assert!(!sw.contains(Interval::from_fps(2).unwrap()));
        assert!((mode.intervals[0].fps() - 60.280).abs() < 0.01);
        assert!((mode.intervals[1].fps() - 120.626).abs() < 0.01);
        assert_eq!(
            (
                mode.intervals[1].numerator.get(),
                mode.intervals[1].denominator.get()
            ),
            (82901, 10_000_000)
        );
    }

    #[test]
    fn the_planner_plans_raw_capture_at_exact_rates() {
        let modes = ov9782();
        let pbaa = FourCc::new(*b"pBAA");
        let mode = capture_mode(&modes[0], pbaa.to_u32()).unwrap();
        let device = ProbedDevice {
            identity: DeviceIdentity {
                display: "ov9782".into(),
                keys: vec!["native:ov9782".into()],
            },
            backends: vec![ProbedBackend {
                kind: BackendKind::Native,
                handle: BackendHandle::Native {
                    key: "bridge:/dev/v4l-subdev2".into(),
                },
                descriptor: CaptureDescriptor::new([mode]),
                properties: Vec::new(),
            }],
        };
        for fps in [30, 60, 120] {
            let req = FrameRequirements::formats([pbaa])
                .min_fps(fps)
                .priority(Priority::Power);
            let plan = crate::planner::plan_frames(&device, &req).unwrap();
            assert_eq!(plan.backend, BackendKind::Native);
            assert_eq!(plan.interval, Interval::from_fps(fps));
            assert!(plan.to_string().contains("native pBAA 1280x800"), "{plan}");
        }
        // Latency first: the fastest rate the mode has.
        let plan =
            crate::planner::plan_frames(&device, &FrameRequirements::formats([pbaa])).unwrap();
        assert!((plan.interval.unwrap().fps() - 120.626).abs() < 0.01);
    }
}
