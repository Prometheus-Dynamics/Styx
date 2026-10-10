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
    BackendFrameMeta, ClockConversion, ClockSource, NativeFrameMeta, TimestampClock,
};
use styx_native::styx_sensor::Control as SensorControl;
use styx_native::{
    CameraControls, CameraInfo, CameraOptions, NativeError, NativeFrame, SensorLibrary, SensorMode,
    StreamSettings,
};

use super::control_plane::ControlPlane;
use super::handle::{CaptureHandle, CaptureQueue, WorkerHandle};
use super::handle_metrics::deliver;
use super::request::CaptureError;
use super::tunables::StyxConfig;
use crate::metrics::StageMetrics;
use crate::{BackendHandle, BackendKind, DeviceIdentity, ProbedBackend, ProbedDevice};

/// Control ids of native cameras (the control plane's `ControlId`s).
///
/// On raw modes exposure, gain and frame duration go to the sensor and land on the frame the
/// control schedule predicts (every frame reports what produced it, `NativeFrameMeta`). On
/// processed (`NV12`/`RG24`) modes the 3A loop owns the sensor: exposure time and gain fix that
/// value for AE (0 hands it back), and the AE/AWB controls below apply; the frame rate is the
/// one the capture started with.
pub mod controls {
    use styx_capture::prelude::ControlId;

    /// Exposure time in microseconds (`Uint`). Processed modes: fixed for AE, 0 = automatic.
    pub const EXPOSURE_TIME_US: ControlId = ControlId(0xF400_0001);
    /// Total gain, analogue first then digital, as a ratio (`Float`). Processed modes: fixed
    /// for AE, 0 = automatic.
    pub const GAIN: ControlId = ControlId(0xF400_0002);
    /// Frame duration in microseconds (`Uint`); sets the frame length exactly. Raw modes.
    pub const FRAME_DURATION_US: ControlId = ControlId(0xF400_0003);
    /// Frame rate in frames per second (`Float`); sets the closest frame length. Raw modes.
    pub const FRAME_RATE: ControlId = ControlId(0xF400_0004);
    /// Processed modes: automatic exposure on or off (`Bool`; off holds the current exposure
    /// and gain).
    pub const AE_ENABLE: ControlId = ControlId(0xF400_0005);
    /// Processed modes: exposure compensation in stops (`Float`).
    pub const EXPOSURE_VALUE: ControlId = ControlId(0xF400_0006);
    /// Processed modes: automatic white balance on or off (`Bool`).
    pub const AWB_ENABLE: ControlId = ControlId(0xF400_0007);
    /// Processed modes: white balance colour temperature in kelvin (`Uint`), used while AWB
    /// is off; read: AWB's estimate for the latest frame.
    pub const COLOUR_TEMPERATURE: ControlId = ControlId(0xF400_0008);
    /// Processed modes: manual red gain (`Float`, relative to green), used while AWB is off.
    pub const RED_GAIN: ControlId = ControlId(0xF400_0009);
    /// Processed modes: manual blue gain (`Float`, relative to green), used while AWB is off.
    pub const BLUE_GAIN: ControlId = ControlId(0xF400_000A);
    /// AE state of a processed mode's 3A loop after the latest frame, as libcamera's
    /// `AeState` (`Int`, read only): 1 searching, 2 converged (AE locked).
    pub const AE_STATE: ControlId = ControlId(0xF400_0010);
    /// Processed modes: flicker avoidance of AE (`Int`): 0 off, 1 50 Hz mains, 2 60 Hz mains,
    /// 3 automatic (`NativeFlicker::control_value`; the default from
    /// `NativeIspConfig::flicker`).
    pub const AE_FLICKER_MODE: ControlId = ControlId(0xF400_0011);
    /// Processed modes: the light flicker period automatic flicker avoidance detected, in
    /// microseconds (`Int`, read only; 0 none yet; 10000 for 50 Hz mains), as libcamera's
    /// `AeFlickerDetected`.
    pub const AE_FLICKER_DETECTED: ControlId = ControlId(0xF400_0012);
    /// Processed modes: deflicker (`Int`): 0 off, 1 on, 2 with flicker avoidance
    /// (`NativeDeflicker::control_value`; the default from `NativeIspConfig::deflicker`).
    pub const AE_DEFLICKER_MODE: ControlId = ControlId(0xF400_0013);
    /// Processed modes of a camera with a focus lens: what drives the lens (`Int`, as
    /// libcamera's `AfMode`): 0 manual (`LENS_POSITION`), 1 auto (a scan per `AF_TRIGGER`),
    /// 2 continuous (the default).
    pub const AF_MODE: ControlId = ControlId(0xF400_0020);
    /// Auto mode: 0 starts a scan, 1 cancels it (`Int`, libcamera's `AfTrigger`).
    pub const AF_TRIGGER: ControlId = ControlId(0xF400_0021);
    /// What AF reports after the latest frame (`Int`, read only, libcamera's `AfState`):
    /// 0 idle, 1 scanning, 2 focused, 3 failed.
    pub const AF_STATE: ControlId = ControlId(0xF400_0022);
    /// The lens position in dioptres (`Float`, 1 / metres; 0 is infinity): set in manual
    /// mode; read: where AF or the manual setting put it.
    pub const LENS_POSITION: ControlId = ControlId(0xF400_0023);
    /// AF windows in output pixels (`Rects`, up to 10), used while `AF_METERING` is 1.
    pub const AF_WINDOWS: ControlId = ControlId(0xF400_0024);
    /// 0: AF looks at the middle of the image (default); 1: at `AF_WINDOWS` (`Int`).
    pub const AF_METERING: ControlId = ControlId(0xF400_0025);
    /// Focus range scans cover (`Int`): 0 normal, 1 macro, 2 full.
    pub const AF_RANGE: ControlId = ControlId(0xF400_0026);
    /// AF speed (`Int`): 0 normal, 1 fast.
    pub const AF_SPEED: ControlId = ControlId(0xF400_0027);
    /// Processed modes on the PiSP with the main output at the mode's size, or on the software
    /// ISP at a sensor mode's size: deliver this region of the frame at full resolution
    /// (`Rect` in frame pixels, rounded out to even pixels, at least 16x16; zero size: the
    /// whole frame). It applies from the next frame the ISP processes; frames carry the region
    /// they show as `FrameMeta::crop`. The overview keeps seeing the whole frame
    /// (`NativeIspConfig::overview`). The software ISP then processes only the region.
    pub const OUTPUT_CROP: ControlId = crate::capture_api::OUTPUT_CROP;
    /// The first `region_crop` control (`REGION_CROP_BASE + index`).
    pub const REGION_CROP_BASE: ControlId = ControlId(0xF400_0040);

    /// Processed modes on the PiSP with regions (`NativeIspConfig::regions`): where region
    /// `index` (1 to `MAX_NATIVE_REGIONS`, the `CompanionKind::Region` companion of that index)
    /// is (`Rect` in frame pixels, rounded out to even pixels, at least 16x16; zero size: none,
    /// no companion). Applies from the next frame the ISP processes. `None` for index 0 (the
    /// main output's is `OUTPUT_CROP`) or beyond the last.
    pub fn region_crop(index: u8) -> Option<ControlId> {
        (1..=super::super::MAX_NATIVE_REGIONS as u8)
            .contains(&index)
            .then(|| ControlId(REGION_CROP_BASE.0 + u32::from(index)))
    }

    /// The region index of a `region_crop` control.
    pub fn region_crop_index(id: ControlId) -> Option<u8> {
        let index = u8::try_from(id.0.checked_sub(REGION_CROP_BASE.0)?).ok()?;
        (region_crop(index) == Some(id)).then_some(index)
    }
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
        // The processed modes' 3A loop.
        meta(
            controls::AE_ENABLE,
            "ae_enable",
            ControlKind::Bool,
            ControlValue::Bool(false),
            ControlValue::Bool(true),
            ControlValue::Bool(true),
            ControlValue::Bool(true),
        ),
        meta(
            controls::EXPOSURE_VALUE,
            "exposure_value",
            ControlKind::Float,
            ControlValue::Float(-8.0),
            ControlValue::Float(8.0),
            ControlValue::Float(0.0),
            ControlValue::Float(0.1),
        ),
        meta(
            controls::AWB_ENABLE,
            "awb_enable",
            ControlKind::Bool,
            ControlValue::Bool(false),
            ControlValue::Bool(true),
            ControlValue::Bool(true),
            ControlValue::Bool(true),
        ),
        meta(
            controls::COLOUR_TEMPERATURE,
            "colour_temperature",
            ControlKind::Uint,
            ControlValue::Uint(1000),
            ControlValue::Uint(20_000),
            ControlValue::Uint(0),
            ControlValue::Uint(1),
        ),
        meta(
            controls::RED_GAIN,
            "red_gain",
            ControlKind::Float,
            ControlValue::Float(0.0),
            ControlValue::Float(8.0),
            ControlValue::Float(0.0),
            ControlValue::Float(0.01),
        ),
        meta(
            controls::BLUE_GAIN,
            "blue_gain",
            ControlKind::Float,
            ControlValue::Float(0.0),
            ControlValue::Float(8.0),
            ControlValue::Float(0.0),
            ControlValue::Float(0.01),
        ),
        meta(
            controls::AE_FLICKER_MODE,
            "ae_flicker_mode",
            ControlKind::Int,
            ControlValue::Int(0),
            ControlValue::Int(3),
            ControlValue::Int(3),
            ControlValue::Int(1),
        ),
        meta(
            controls::AE_DEFLICKER_MODE,
            "ae_deflicker_mode",
            ControlKind::Int,
            ControlValue::Int(0),
            ControlValue::Int(2),
            ControlValue::Int(2),
            ControlValue::Int(1),
        ),
        ControlMeta {
            access: Access::ReadOnly,
            ..meta(
                controls::AE_FLICKER_DETECTED,
                "ae_flicker_detected",
                ControlKind::Int,
                ControlValue::Int(0),
                ControlValue::Int(1_000_000),
                ControlValue::Int(0),
                ControlValue::Int(1),
            )
        },
        ControlMeta {
            access: Access::ReadOnly,
            ..meta(
                controls::AE_STATE,
                "ae_state",
                ControlKind::Int,
                ControlValue::Int(0),
                ControlValue::Int(2),
                ControlValue::Int(1),
                ControlValue::Int(1),
            )
        },
    ]
    .into_iter()
    .chain(
        match super::native_isp::isp_name(info) {
            "pisp" => Some(super::native_isp::crop_metas(info, true)),
            "software" => Some(super::native_isp::crop_metas(info, false)),
            _ => None,
        }
        .into_iter()
        .flatten(),
    )
    .chain(info.lens.as_ref().map_or_else(Vec::new, |l| {
        let m = &l.description.map;
        let limits = if m.len() >= 4 {
            (m[0], m[m.len() - 2])
        } else {
            (0.0, 12.0)
        };
        super::native_isp::af_metas(limits)
    }))
    .collect()
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
    let binned = super::native_isp::isp_name(info) == "software";
    let processed = super::native_isp::processed_modes(&modes, binned);
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

/// The conversion from a native frame's `CLOCK_MONOTONIC` timestamp to `clock`, sampled now.
/// Sample it once per frame and pass it to every lease of that frame (its pyramid companions
/// share the timestamp, so they must convert identically). `None` keeps monotonic.
pub(crate) fn native_conversion(clock: ClockSource) -> Option<ClockConversion> {
    clock.conversion_from(TimestampClock::Monotonic)
}

/// Stamps a native frame's timestamp, `CLOCK_MONOTONIC` (the V4L2 buffer's timestamp from the
/// kernel, or the PiSP's or software ISP's own), converted by `conversion` from
/// [`native_conversion`]. The default, [`ClockSource::Native`], keeps monotonic.
pub(crate) fn stamp_clock(meta: FrameMeta, conversion: Option<ClockConversion>) -> FrameMeta {
    meta.in_clock(TimestampClock::Monotonic, conversion)
}

/// Wraps a native frame as a frame lease (no copy), stamped in `clock`.
pub(crate) fn frame_lease(
    frame: NativeFrame,
    live: &crate::metrics::CaptureMetrics,
    clock: ClockSource,
) -> Option<FrameLease> {
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
    let meta = stamp_clock(
        FrameMeta::new(format, frame.timestamp.as_nanos() as u64)
            .with_backend(BackendFrameMeta::Native(native))
            .with_capture_instant(frame.dequeued),
        native_conversion(clock),
    );
    let layout = PlaneLayout {
        offset: 0,
        len,
        stride: frame.stride as usize,
    };
    Some(FrameLease::from_external(
        meta,
        smallvec![layout],
        Arc::new(live.track(frame.into_backing(len))),
    ))
}

/// Applies one control through the native control schedule.
pub(crate) fn apply_control(
    controls: &CameraControls,
    id: ControlId,
    value: &ControlValue,
) -> Result<(), CaptureError> {
    apply_control_landing(controls, id, value).map(|_| ())
}

/// [`apply_control`], and the first frame (sensor sequence) predicted to use the value.
pub(crate) fn apply_control_landing(
    controls: &CameraControls,
    id: ControlId,
    value: &ControlValue,
) -> Result<Option<u64>, CaptureError> {
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
    r.map(|landings| landings.iter().map(|l| l.frame).max())
        .map_err(|e| CaptureError::control_apply(e.to_string()))
}

/// The sensor controls a processed mode's fixed value is written to: exposure, or the analogue
/// and digital gains (a total gain is written analogue first). `None` for every other control
/// and for a value of 0, which hands the control back to AE: no frame is then fixed.
pub(crate) fn fixed_sensor_controls(
    id: ControlId,
    value: &ControlValue,
) -> Option<&'static [SensorControl]> {
    let positive = match value {
        ControlValue::Uint(v) => *v > 0,
        ControlValue::Int(v) => *v > 0,
        ControlValue::Float(v) => *v > 0.0,
        _ => false,
    };
    if !positive {
        return None;
    }
    match id {
        controls::EXPOSURE_TIME_US => Some(&[SensorControl::Exposure]),
        controls::GAIN => Some(&[SensorControl::AnalogGain, SensorControl::DigitalGain]),
        _ => None,
    }
}

/// The first frame a processed mode's fixed exposure or gain takes effect on: the 3A loop writes
/// it during the frame in progress, so the sensor's landing for that write (current frame plus
/// the control's delay, see [`CameraControls::landing_now`]). A predicted landing: the loop's
/// write can miss the frame and land one later, and a sensor with embedded data reports the
/// frame's values with `NativeFrameMeta::verified`. `None` where no frame is fixed (AE, EV, AWB).
pub(crate) fn processed_landing(
    controls: &CameraControls,
    id: ControlId,
    value: &ControlValue,
) -> Option<u64> {
    fixed_sensor_controls(id, value).map(|cs| {
        cs.iter()
            .map(|c| controls.landing_now(*c))
            .max()
            .unwrap_or(0)
    })
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
    // A request without a rate runs at the planner's default (30 fps within the mode's range),
    // not the sensor's default frame length.
    let interval = interval.or_else(|| crate::planner::default_interval(&mode));
    let provider = styx_native::NativeProvider::new(SensorLibrary::system())
        .with_options(CameraOptions::default());
    if super::native_isp::is_processed(mode.format.code) {
        let camera = super::native_isp::open_for_isp(&provider, key, config)?;
        // The 3A loop owns exposure and gain; initial controls go to the loop.
        return super::native_isp::start_processed(
            camera, mode, interval, &initial, descriptor, config, queue,
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
    let clock = capture.timestamp_clock;
    // A queue the supervisor passed in belongs to the consumer and outlives this capture (a
    // reconnect starts the next one on it): only a queue made here is closed when it ends.
    let owns_queue = queue.is_none();
    let (tx, rx) = queue.unwrap_or_else(|| {
        styx_core::queue::bounded_with(capture.queue_depth.max(1), capture.queue_overflow)
    });
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let worker_error = Arc::new(Mutex::new(None));
    let worker_error_for_thread = worker_error.clone();
    let send_timeout = Duration::from_millis(capture.queue_send_timeout_ms);
    let poll = Duration::from_millis(capture.idle_poll_ms.clamp(5, 100));
    let live = crate::metrics::CaptureMetrics::default();
    let live_worker = live.clone();
    let worker = thread::Builder::new()
        .name("styx-native-capture".into())
        .spawn(move || {
            live_worker.register_thread();
            crate::trace::debug!(backend = "native", "capture worker started");
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                match stream.next_blocking(poll) {
                    Ok(Some(frame)) => {
                        let Some(lease) = frame_lease(frame, &live_worker, clock) else {
                            continue;
                        };
                        if deliver(&live_worker, &tx, lease, "native", send_timeout) {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(NativeError::Timeout) => {}
                    Err(e) => {
                        crate::trace::warn!(backend = "native", error = %e, "capture failed");
                        *worker_error_for_thread.lock() = Some(native_err(e));
                        break;
                    }
                }
            }
            drop(stream);
            if let Err(e) = camera.close() {
                crate::trace::warn!(backend = "native", error = %e, "closing the camera");
            }
            if owns_queue {
                tx.close();
            }
            crate::trace::debug!(backend = "native", "capture worker stopped");
        })
        .map_err(|e| CaptureError::Backend(format!("native worker: {e}")))?;
    Ok(CaptureHandle {
        backend: BackendKind::Native,
        control: ControlPlane::Native {
            controls,
            processed: None,
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
        sequence_gaps: live.sequence_gaps(),
        live,
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
    fn native_frames_stamp_monotonic_unless_another_clock_is_asked_for() {
        let res = Resolution::new(2, 2).unwrap();
        let format = MediaFormat::new(FourCc::GREY, res, ColorSpace::Unknown);
        let stamp =
            |clock| stamp_clock(FrameMeta::new(format, 1_000_000), native_conversion(clock));

        // The default keeps the kernel's CLOCK_MONOTONIC value as it is.
        let native = stamp(ClockSource::Native);
        assert_eq!(native.clock, Some(TimestampClock::Monotonic));
        assert_eq!(native.timestamp, 1_000_000);
        let explicit = stamp(ClockSource::Monotonic);
        assert_eq!(explicit.clock, Some(TimestampClock::Monotonic));
        assert_eq!(explicit.timestamp, 1_000_000);

        // Converted clocks: a frame stamped now (on CLOCK_MONOTONIC) reads as now on the target
        // clock, within the sampling error of the two clock reads.
        let mono_now = TimestampClock::Monotonic.now_ns().unwrap();
        let now_stamp =
            |clock| stamp_clock(FrameMeta::new(format, mono_now), native_conversion(clock));
        let boot = now_stamp(ClockSource::Boottime);
        assert_eq!(boot.clock, Some(TimestampClock::Boottime));
        let boot_now = TimestampClock::Boottime.now_ns().unwrap();
        assert!(
            boot.timestamp.abs_diff(boot_now) < 10_000_000,
            "{}",
            boot.timestamp
        );
        let back = boot.timestamp_in(TimestampClock::Monotonic).unwrap();
        assert!(back.abs_diff(mono_now) < 10_000_000, "{back}");
        let real = now_stamp(ClockSource::Realtime);
        assert_eq!(real.clock, Some(TimestampClock::Realtime));
        let real_now = TimestampClock::Realtime.now_ns().unwrap();
        assert!(
            real.timestamp.abs_diff(real_now) < 10_000_000,
            "{}",
            real.timestamp
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
            let req = crate::planner::FrameRequest::formats([pbaa]).fps(fps);
            let plan = crate::planner::plan_frames(&device, &req).unwrap();
            assert_eq!(plan.backend, BackendKind::Native);
            assert_eq!(plan.interval, Interval::from_fps(fps));
            assert!(plan.to_string().contains("native pBAA 1280x800"), "{plan}");
        }
        // At least a rate: the fastest rate the mode has.
        let req = crate::planner::FrameRequest::formats([pbaa]).fps_at_least(30);
        let plan = crate::planner::plan_frames(&device, &req).unwrap();
        assert!((plan.interval.unwrap().fps() - 120.626).abs() < 0.01);
        // No rate asked for, whatever the delivery: 30 fps, not the fastest.
        for req in [
            crate::planner::FrameRequest::formats([pbaa]),
            crate::planner::FrameRequest::formats([pbaa]).every_frame(3),
        ] {
            let plan = crate::planner::plan_frames(&device, &req).unwrap();
            assert_eq!(plan.interval, Interval::from_fps(30), "{req:?}");
            let shared = crate::planner::plan_many(&device, std::slice::from_ref(&req)).unwrap();
            assert_eq!(shared.interval, Interval::from_fps(30), "{req:?}");
        }
    }

    #[test]
    fn only_a_fixed_exposure_or_gain_fixes_a_frame() {
        let fixed = |id, v| fixed_sensor_controls(id, &v);
        assert_eq!(
            fixed(controls::EXPOSURE_TIME_US, ControlValue::Uint(5000)),
            Some(&[SensorControl::Exposure][..])
        );
        assert_eq!(
            fixed(controls::GAIN, ControlValue::Float(2.0)),
            Some(&[SensorControl::AnalogGain, SensorControl::DigitalGain][..])
        );
        // 0 hands the control back to AE: no frame is fixed.
        assert_eq!(
            fixed(controls::EXPOSURE_TIME_US, ControlValue::Uint(0)),
            None
        );
        assert_eq!(fixed(controls::GAIN, ControlValue::Float(0.0)), None);
        // AE, EV, AWB and colour temperature fix no frame.
        assert_eq!(fixed(controls::AE_ENABLE, ControlValue::Bool(false)), None);
        assert_eq!(
            fixed(controls::EXPOSURE_VALUE, ControlValue::Float(1.0)),
            None
        );
        assert_eq!(fixed(controls::AWB_ENABLE, ControlValue::Bool(false)), None);
        assert_eq!(
            fixed(controls::COLOUR_TEMPERATURE, ControlValue::Uint(5000)),
            None
        );
    }
}
