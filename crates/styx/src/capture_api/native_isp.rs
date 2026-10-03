//! Processed frames from native cameras: the 3A loop of `styx-pipeline` closed over the
//! camera, with the PiSP (front end statistics, back end NV12/RGB, raw frames handed over as
//! dma-bufs) where the camera's receiver has one, else the software ISP.
//!
//! The native backend lists `NV12` and `RG24` modes next to the raw ones; the planner prices
//! them by ISP (see `planner::cost`) and this module runs them. PiSP frames are leased straight
//! from the back end's output buffers (mapped once, exported as dma-bufs, returned to the back
//! end when the lease drops); software ISP frames are written into recycled heap buffers.

mod loop_controls;
mod pisp_worker;
mod still_process;
mod still_runner;

pub(crate) use loop_controls::LoopControls;
pub(crate) use still_runner::StillJob;

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use smallvec::{SmallVec, smallvec};
use styx_capture::prelude::*;
use styx_core::prelude::{BackendFrameMeta, ExternalBacking, NativeFrameMeta, TimestampClock};
use styx_native::{CameraInfo, NativeCamera, StreamSettings};
use styx_pipeline::SensorValues;
use styx_pipeline::device::{
    IspKind, PispOptions, PispPipeline, SoftPipeline, find_tuning, isp_kind, soft_capture_memory,
};
use styx_pisp::device::OutputMemory;
use styx_softisp::{OutputBuffers, Scale};

use super::control_plane::ControlPlane;
use super::handle::{CaptureHandle, CaptureQueue, WorkerHandle, enqueue_capture_frame};
use super::request::CaptureError;
use super::tunables::StyxConfig;
use crate::BackendKind;
use crate::metrics::StageMetrics;

/// Formats the native backend makes from raw frames.
pub(crate) const PROCESSED: [FourCc; 2] = [FourCc::NV12, FourCc::RG24];

/// Whether `code` is one of the processed formats.
pub(crate) fn is_processed(code: FourCc) -> bool {
    PROCESSED.contains(&code)
}

/// The ISP a camera's processed modes run on (`pisp` or `software`), for the probe's
/// properties and the planner's costs.
pub(crate) fn isp_name(info: &CameraInfo) -> &'static str {
    isp_kind(info).name()
}

/// `NV12` and `RG24` modes at each size the sensor's raw modes have, with their intervals;
/// with `binned` (the software ISP) also at half each size, made by turning each 2x2 quad of
/// the mosaic into a pixel (no demosaic: about 40% of the full-size ISP time, and the
/// planner picks these for consumers that want small frames).
pub(crate) fn processed_modes(raw: &[Mode], binned: bool) -> Vec<Mode> {
    let mut out: Vec<Mode> = Vec::new();
    let full: Vec<Resolution> = raw.iter().map(|m| m.format.resolution).collect();
    for m in raw {
        let res = m.format.resolution;
        let half = Resolution::new(res.width.get() / 2, res.height.get() / 2)
            .filter(|h| binned && h.width.get() % 2 == 0 && h.height.get() % 2 == 0);
        for res in std::iter::once(res).chain(half.filter(|h| !full.contains(h))) {
            if out.iter().any(|o| o.format.resolution == res) {
                continue;
            }
            for code in PROCESSED {
                let format = MediaFormat::new(code, res, ColorSpace::Srgb);
                out.push(Mode {
                    id: ModeId {
                        format,
                        interval: None,
                    },
                    format,
                    intervals: m.intervals.clone(),
                    interval_stepwise: m.interval_stepwise,
                });
            }
        }
    }
    out
}

/// The sensor size a processed mode of `res` captures at and the software ISP's scale: the
/// same size when the sensor has a mode of it, else twice it (a binned mode).
pub(crate) fn soft_capture_size(info: &CameraInfo, (w, h): (u32, u32)) -> ((u32, u32), Scale) {
    let has = |w: u32, h: u32| info.modes.iter().any(|m| (m.width, m.height) == (w, h));
    if !has(w, h) && has(2 * w, 2 * h) {
        ((2 * w, 2 * h), Scale::Half)
    } else {
        ((w, h), Scale::Full)
    }
}

/// Opens camera `key` for processed capture: with the software ISP, raw frames go into cached
/// dma-heap buffers when the system has the heap ([`soft_capture_memory`]; the CPU reads the
/// receiver's own MMAP buffers uncached), unless the configuration asks for the driver's
/// buffers. Falls back to the driver's buffers if the heap cannot be used. Returns the camera
/// and whether its raw buffers are cached.
pub(crate) fn open_for_isp(
    provider: &styx_native::NativeProvider,
    key: &str,
    config: &StyxConfig,
) -> Result<(NativeCamera, bool), CaptureError> {
    let (cameras, _) = provider.discover_cameras();
    let memory = match cameras.into_iter().find(|c| c.key == key) {
        Some(info)
            if isp_kind(&info) == IspKind::Software && !config.backends.native.driver_buffers =>
        {
            Some((info, soft_capture_memory()))
        }
        _ => None,
    };
    match memory {
        Some((info, memory @ styx_native::BufferMemory::DmaHeap(_))) => {
            let options = styx_native::CameraOptions {
                memory,
                ..Default::default()
            };
            match NativeCamera::open(info, options) {
                Ok(c) => Ok((c, true)),
                Err(e) => {
                    tracing::warn!(backend = "native", error = %e, "cached capture buffers unavailable");
                    provider.open_camera(key).map(|c| (c, false)).map_err(err)
                }
            }
        }
        _ => provider.open_camera(key).map(|c| (c, false)).map_err(err),
    }
}

fn err(e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Backend(format!("native ISP: {e}"))
}

/// A heap buffer of the software path, recycled through `returns` when its lease drops.
struct HeapBacking {
    data: Option<Vec<u8>>,
    returns: mpsc::Sender<Vec<u8>>,
}

impl ExternalBacking for HeapBacking {
    fn plane_data(&self, _index: usize) -> Option<&[u8]> {
        self.data.as_deref()
    }

    fn backing_bytes(&self) -> Option<usize> {
        self.data.as_ref().map(Vec::len)
    }

    fn backing_kind(&self) -> &'static str {
        "softisp_heap"
    }
}

impl Drop for HeapBacking {
    fn drop(&mut self) {
        if let Some(d) = self.data.take() {
            let _ = self.returns.send(d);
        }
    }
}

/// Plane layouts of a `code` image of `w`x`h` with `stride` bytes per row in one buffer.
fn layouts(code: FourCc, h: usize, stride: usize) -> SmallVec<[PlaneLayout; 3]> {
    if code == FourCc::NV12 {
        smallvec![
            PlaneLayout {
                offset: 0,
                len: stride * h,
                stride,
            },
            PlaneLayout {
                offset: stride * h,
                len: stride * h / 2,
                stride,
            },
        ]
    } else {
        smallvec![PlaneLayout {
            offset: 0,
            len: stride * h,
            stride,
        }]
    }
}

fn native_meta(sequence: u64, s: &SensorValues) -> NativeFrameMeta {
    NativeFrameMeta {
        sequence: sequence as u32,
        bytes_used: 0,
        error: false,
        exposure_ns: s.exposure.as_nanos() as u64,
        analog_gain: s.analogue_gain as f32,
        digital_gain: s.digital_gain as f32,
        frame_duration_ns: s.frame_duration.as_nanos() as u64,
        frame_length: 0,
        verified: s.verified,
    }
}

fn frame_meta(mode: &Mode, sequence: u64, timestamp: Duration, s: &SensorValues) -> FrameMeta {
    let mut meta = FrameMeta::new(mode.format, timestamp.as_nanos() as u64)
        .with_backend(BackendFrameMeta::Native(native_meta(sequence, s)))
        .with_capture_instant(std::time::Instant::now());
    meta.clock = Some(TimestampClock::Monotonic);
    meta
}

/// A software path frame's raw rows held for a still.
fn hold_soft(
    f: &styx_pipeline::device::SoftFrame,
    format: styx_softisp::RawFormat,
) -> Box<styx_pipeline::still::HeldRaw> {
    let stride = f.raw.stride as usize;
    let len = (stride * format.height as usize).min(f.raw.data().len());
    Box::new(styx_pipeline::still::HeldRaw {
        sequence: f.sensor.frame,
        timestamp: f.raw.timestamp,
        width: format.width,
        height: format.height,
        stride,
        packing: format.packing,
        cfa: format.pattern,
        bits: format.packing.bit_depth(),
        data: f.raw.data()[..len].to_vec(),
        sensor: f.sensor,
        isp: f.output.applied.clone(),
        params: Box::new(f.output.step.params.clone()),
    })
}

/// Back end buffers per PiSP output: [`NativeIspConfig::output_buffers`] covers the back
/// end's own and a capture with the default queue and extra buffers (2 + 2) plus slow holders
/// outside it (a frame server's leases); a deeper queue or more extra buffers (the planner's
/// shared captures reserve every consumer's queue and the frame it works on) come on top, so
/// consumers holding what the planner allows them never leave the camera without a buffer.
/// With them all the outputs' buffers (`set_bytes` per buffer of every output) take at most
/// half of `free` bytes (the contiguous memory they come from, when known; the front end's
/// raw buffers and the back end's temporal denoise come from it too), but never fewer than
/// `output_buffers`.
///
/// [`NativeIspConfig::output_buffers`]: super::NativeIspConfig::output_buffers
pub(super) fn pisp_output_buffers(config: &StyxConfig, set_bytes: u64, free: Option<u64>) -> u32 {
    let capture = config.capture_tunables();
    let planned = capture.queue_depth + capture.extra_buffers;
    let baseline =
        super::tunables::DEFAULT_QUEUE_DEPTH + super::tunables::DEFAULT_CAPTURE_EXTRA_BUFFERS;
    let mut extra = u32::try_from(planned.saturating_sub(baseline)).unwrap_or(u32::MAX);
    let base = config.backends.native.output_buffers.max(2);
    if let Some(free) = free {
        let fits = (free / 2 / set_bytes.max(1)).saturating_sub(u64::from(base));
        extra = extra.min(u32::try_from(fits).unwrap_or(u32::MAX));
    }
    base.saturating_add(extra).min(MAX_OUTPUT_BUFFERS.max(base))
}

/// Free contiguous memory (`CmaFree`), where the back end's cached dma-heap buffers come from.
fn cma_free() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib: u64 = info
        .lines()
        .find_map(|l| l.strip_prefix("CmaFree:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kib * 1024)
}

/// Most buffers per back end output (V4L2 allows 64; each is a frame of the output's size).
const MAX_OUTPUT_BUFFERS: u32 = 32;

/// Starts processed capture on an opened camera.
#[allow(clippy::too_many_arguments)]
pub(super) fn start_processed(
    (camera, _cached): (NativeCamera, bool),
    mode: Mode,
    interval: Option<Interval>,
    initial: &[(ControlId, ControlValue)],
    descriptor: CaptureDescriptor,
    config: &StyxConfig,
    queue: Option<CaptureQueue>,
) -> Result<CaptureHandle, CaptureError> {
    let (w, h) = (
        mode.format.resolution.width.get(),
        mode.format.resolution.height.get(),
    );
    let fraction =
        interval.map(|i| styx_native::Fraction::new(i.numerator.get(), i.denominator.get()));
    let (tuning, source) = find_tuning(&camera.info().description);
    let kind = isp_kind(camera.info());
    tracing::info!(backend = "native", isp = kind.name(), tuning = %source, "processed capture");
    let capture = config.capture_tunables();
    // A queue the supervisor passed in belongs to the consumer and outlives this capture (a
    // reconnect starts the next one on it): only a queue made here is closed when it ends.
    let owns_queue = queue.is_none();
    let (tx, rx) = queue.unwrap_or_else(|| {
        styx_core::queue::bounded_with(capture.queue_depth.max(1), capture.queue_overflow)
    });
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    // The 3A loop's controls (initial ones applied before the start) and its state.
    let loop_controls = Arc::new(LoopControls::with_flicker(
        config.backends.native.flicker,
        config.backends.native.deflicker,
    ));
    for (id, value) in initial {
        loop_controls
            .apply(*id, value)
            .unwrap_or(Err(CaptureError::ControlUnsupported))?;
    }
    let loop_worker = Arc::clone(&loop_controls);
    let worker_error = Arc::new(Mutex::new(None));
    let werr = Arc::clone(&worker_error);
    let send_timeout = Duration::from_millis(capture.queue_send_timeout_ms);
    let timeout = Duration::from_secs(2);
    let code = mode.format.code;
    let worker_mode = mode.clone();
    let still_ctx = Arc::new(still_process::StillContext {
        kind,
        source: styx_pipeline::still::StillSource {
            model: camera.info().location.sensor_name.clone(),
            unique_camera_model: format!("Styx {} ({source})", camera.info().location.sensor_name),
            calibrations: styx_pipeline::still::dng_calibrations(&tuning),
        },
        threads: config
            .backends
            .native
            .soft_threads
            .unwrap_or_else(crate::planner::cost::default_softisp_threads),
    });
    let mut still = still_runner::StillRunner::new(
        Arc::clone(&loop_controls),
        Box::new(move || still_process::StillProcessor::spawn(Arc::clone(&still_ctx))),
    );
    let (controls, worker): (styx_native::CameraControls, thread::JoinHandle<()>) = match kind {
        IspKind::Pisp => {
            let settings = StreamSettings {
                width: w,
                height: h,
                fourcc: None,
                code: None,
                interval: fraction,
            };
            let (specs, second_kind) = pisp_worker::output_specs(&mode, &config.backends.native)?;
            let setup = |i: usize| specs[i].map(|s| s.setup()).transpose();
            let options = PispOptions {
                outputs: [setup(0)?, setup(1)?],
                output_memory: if config.backends.native.driver_buffers {
                    OutputMemory::Driver
                } else {
                    OutputMemory::CachedHeap
                },
                temporal_denoise: config.backends.native.temporal_denoise,
                spatial_denoise: f64::from(config.backends.native.spatial_denoise_percent) / 100.0,
                be_buffers: pisp_output_buffers(
                    config,
                    specs.iter().flatten().map(|s| s.bytes()).sum(),
                    cma_free().filter(|_| !config.backends.native.driver_buffers),
                ),
                ..PispOptions::nv12_and_half_rgb(w, h)
            };
            let mut p = PispPipeline::open(camera, &settings, &tuning, options).map_err(err)?;
            if let Some(c) = loop_controls.take() {
                p.controller().set_controls(c);
            }
            p.start().map_err(err)?;
            let controls = p.controls().clone();
            let stride = |i: usize| p.output_format(i).map_or(0, |f| f.stride as usize);
            let strides = [stride(0), stride(1)];
            let worker = pisp_worker::spawn(
                p,
                pisp_worker::Worker {
                    specs,
                    second_kind,
                    strides,
                    tx,
                    owns_queue,
                    stop: stop_rx,
                    error: werr,
                    send_timeout,
                    timeout,
                    loop_controls: loop_worker,
                    still,
                },
            )?;
            (controls, worker)
        }
        IspKind::Software => {
            let ((cw, ch), scale) = soft_capture_size(camera.info(), (w, h));
            let settings = StreamSettings {
                width: cw,
                height: ch,
                fourcc: None,
                code: None,
                interval: fraction,
            };
            let threads = config
                .backends
                .native
                .soft_threads
                .unwrap_or_else(crate::planner::cost::default_softisp_threads);
            tracing::info!(backend = "native", threads, "software ISP threads");
            let mut p = SoftPipeline::open(camera, &settings, &tuning, threads).map_err(err)?;
            #[cfg(feature = "gpu-isp")]
            if let Some(ctx) = crate::gpu_isp::context() {
                match p.use_gpu(&ctx) {
                    Ok(()) => {
                        tracing::info!(backend = "native", device = %ctx.info().name, "GPU ISP")
                    }
                    Err(e) => {
                        tracing::warn!(backend = "native", error = %e, "GPU ISP refused; software ISP")
                    }
                }
            }
            if let Some(c) = loop_controls.take() {
                p.soft_loop().controller().set_controls(c);
            }
            p.start().map_err(err)?;
            let controls = p.controls().clone();
            let stride = if code == FourCc::NV12 {
                w as usize
            } else {
                w as usize * 3
            };
            let len = layouts(code, h as usize, stride)
                .iter()
                .map(|l| l.offset + l.len)
                .max()
                .unwrap_or(0);
            let worker = thread::Builder::new()
                .name("styx-native-softisp".into())
                .spawn(move || {
                    let (ret_tx, ret_rx) = mpsc::channel::<Vec<u8>>();
                    loop {
                        if stop_rx.try_recv().is_ok() {
                            break;
                        }
                        if let Some(c) = loop_worker.take() {
                            p.soft_loop().controller().set_controls(c);
                        }
                        still.before_frame(&mut p);
                        let mut buf = ret_rx.try_recv().unwrap_or_else(|_| vec![0u8; len]);
                        let out = if code == FourCc::NV12 {
                            let (y, uv) = buf.split_at_mut(stride * h as usize);
                            OutputBuffers::Nv12 {
                                y,
                                y_stride: stride,
                                uv,
                                uv_stride: stride,
                            }
                        } else {
                            OutputBuffers::Rgb24 {
                                data: &mut buf,
                                stride,
                            }
                        };
                        let f = match p.next(timeout, scale, out) {
                            Ok(Some(f)) => f,
                            Ok(None) => break,
                            Err(e) => {
                                *werr.lock() = Some(err(e));
                                break;
                            }
                        };
                        loop_worker.report(&f.output.step.params);
                        let raw = still
                            .wants(&f.sensor)
                            .then(|| hold_soft(&f, p.soft_loop().format()));
                        let step = &f.output.step;
                        let ae = (step.params.ae.total_exposure, step.params.ae.locked);
                        let request = step.sensor;
                        let (sensor, lands) = (f.sensor, f.request_lands);
                        let meta =
                            frame_meta(&worker_mode, f.sensor.frame, f.raw.timestamp, &f.sensor);
                        drop(f);
                        still.after_frame(&mut p, &sensor, (lands, request), ae, raw);
                        let lease = FrameLease::from_external(
                            meta,
                            layouts(code, h as usize, stride),
                            Arc::new(HeapBacking {
                                data: Some(buf),
                                returns: ret_tx.clone(),
                            }),
                        );
                        if enqueue_capture_frame(&tx, lease, "native-softisp", send_timeout) {
                            break;
                        }
                    }
                    if let Err(e) = p.close() {
                        tracing::warn!(backend = "native", error = %e, "closing the software ISP path");
                    }
                    if owns_queue {
                        tx.close();
                    }
                })
                .map_err(|e| err(format!("worker: {e}")))?;
            (controls, worker)
        }
    };
    Ok(CaptureHandle {
        backend: BackendKind::Native,
        control: ControlPlane::Native {
            controls,
            processed: Some(loop_controls),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_buffers_cover_what_the_planner_reserves() {
        let set = 1280 * 800 * 3 / 2 + 640 * 400 * 3;
        let buffers = |c: &StyxConfig| pisp_output_buffers(c, set, None);
        assert_eq!(buffers(&StyxConfig::default()), 6);
        assert_eq!(buffers(&StyxConfig::default().native_output_buffers(8)), 8);
        // A shared capture of two consumers with queues of 3 (the planner: queue 1, extra
        // 3 + 3 + 3 + 3 + 2 = 14): 11 on top of the 6.
        let shared = StyxConfig::new()
            .capture_queue_depth(1)
            .capture_extra_buffers(14);
        assert_eq!(buffers(&shared), 17);
        let huge = StyxConfig::new().capture_extra_buffers(200);
        assert_eq!(buffers(&huge), MAX_OUTPUT_BUFFERS);
        // All of them in at most half the free contiguous memory: 47 MB free on the CM5 holds
        // 10 sets of NV12 1280x800 + RGB 640x400; never fewer than asked.
        assert_eq!(pisp_output_buffers(&shared, set, Some(47 << 20)), 10);
        assert_eq!(pisp_output_buffers(&shared, set, Some(0)), 6);
    }

    #[test]
    fn processed_modes_follow_the_raw_sizes() {
        let res = Resolution::new(1280, 800).unwrap();
        let raw = |code: &[u8; 4]| {
            let format = MediaFormat::new(FourCc::new(*code), res, ColorSpace::Unknown);
            Mode {
                id: ModeId {
                    format,
                    interval: None,
                },
                format,
                intervals: smallvec![Interval::from_fps(30).unwrap()],
                interval_stepwise: None,
            }
        };
        let modes = processed_modes(&[raw(b"pBAA"), raw(b"BA81")], false);
        let codes: Vec<String> = modes.iter().map(|m| m.format.code.to_string()).collect();
        assert_eq!(codes, ["NV12", "RG24"]);
        let binned = processed_modes(&[raw(b"pBAA")], true);
        let sizes: Vec<(u32, u32)> = binned
            .iter()
            .map(|m| {
                (
                    m.format.resolution.width.get(),
                    m.format.resolution.height.get(),
                )
            })
            .collect();
        assert_eq!(sizes, [(1280, 800), (1280, 800), (640, 400), (640, 400)]);
        assert!(modes.iter().all(|m| m.intervals.len() == 1));
        assert!(is_processed(FourCc::NV12) && !is_processed(FourCc::new(*b"pBAA")));
        let l = layouts(FourCc::NV12, 800, 1280);
        assert_eq!((l[1].offset, l[1].len), (1280 * 800, 1280 * 400));
    }

    fn device(isp: &str) -> crate::ProbedDevice {
        use crate::{BackendHandle, DeviceIdentity, ProbedBackend};
        let res = Resolution::new(1280, 800).unwrap();
        let format = MediaFormat::new(FourCc::new(*b"pBAA"), res, ColorSpace::Unknown);
        let raw = Mode {
            id: ModeId {
                format,
                interval: None,
            },
            format,
            intervals: smallvec![Interval::from_fps(30).unwrap()],
            interval_stepwise: None,
        };
        let mut modes = vec![raw.clone()];
        modes.extend(processed_modes(&[raw], isp == "software"));
        crate::ProbedDevice {
            identity: DeviceIdentity {
                display: "ov9782".into(),
                keys: vec!["native:ov9782".into()],
            },
            backends: vec![ProbedBackend {
                kind: BackendKind::Native,
                handle: BackendHandle::Native {
                    key: "bridge:/dev/v4l-subdev2".into(),
                },
                descriptor: CaptureDescriptor::new(modes),
                properties: vec![("isp".into(), isp.into())],
            }],
        }
    }

    #[test]
    fn the_planner_picks_the_isp_for_processed_frames() {
        let pisp = device("pisp");
        for (req, code, luma) in [
            (
                FrameRequirements::formats([FourCc::NV12]),
                FourCc::NV12,
                false,
            ),
            (
                FrameRequirements::formats([FourCc::RG24]),
                FourCc::RG24,
                false,
            ),
            (FrameRequirements::luma(), FourCc::NV12, true),
        ] {
            let plan = crate::planner::plan_frames(&pisp, &req).unwrap();
            assert_eq!(plan.backend, BackendKind::Native);
            assert_eq!(plan.mode.format.code, code, "{plan}");
            assert!(plan.to_string().contains("PiSP"), "{plan}");
            assert!(plan.total.cpu_ms < 2.0, "{plan}");
            assert_eq!(
                plan.steps
                    .iter()
                    .any(|s| s.kind == crate::planner::StepKind::LumaView),
                luma,
                "{plan}"
            );
            // The raw mode through a decoder is not offered (no 3A on that route).
            assert!(
                plan.rejected.iter().any(|r| r.reason.contains("3A")),
                "{plan}"
            );
        }
        // Raw stays raw.
        let raw = FrameRequirements::formats([FourCc::new(*b"pBAA")]);
        let plan = crate::planner::plan_frames(&pisp, &raw).unwrap();
        assert_eq!(plan.mode.format.code, FourCc::new(*b"pBAA"));
        // Without a PiSP the software ISP runs the loop, priced by the megapixel.
        let soft = device("software");
        let plan = crate::planner::plan_frames(&soft, &FrameRequirements::formats([FourCc::NV12]))
            .unwrap();
        assert!(plan.to_string().contains("software ISP"), "{plan}");
        assert!(plan.total.cpu_ms > 2.5, "{plan}");
        assert_eq!(plan.mode.format.resolution.width.get(), 1280, "{plan}");
        // A consumer of small frames gets the binned mode: no demosaic, less than half the CPU.
        let small = FrameRequirements::formats([FourCc::NV12]).output_resolution(640, 400);
        let half = crate::planner::plan_frames(&soft, &small).unwrap();
        assert_eq!(half.mode.format.resolution.width.get(), 640, "{half}");
        assert!(
            half.total.cpu_ms < plan.total.cpu_ms * 0.6,
            "{half} vs {plan}"
        );
    }
}
