//! Processed frames from native cameras: the 3A loop of `styx-pipeline` closed over the
//! camera, with the PiSP (front end statistics, back end NV12/RGB, raw frames handed over as
//! dma-bufs) where the camera's receiver has one, else the software ISP.
//!
//! The native backend lists `NV12` and `RG24` modes next to the raw ones; the planner prices
//! them by ISP (see `planner::cost`) and this module runs them. PiSP frames are leased straight
//! from the back end's output buffers (mapped once, exported as dma-bufs, returned to the back
//! end when the lease drops); software ISP frames are written into recycled heap buffers.

mod ae_controls;
pub(crate) use ae_controls::{EXPOSURE_MODES, METERING_MODES};
mod af_controls;
mod crop_control;
mod loop_controls;
mod output_buffers;
mod pisp_lease;
mod pisp_regions;
mod pisp_sliced;
mod pisp_worker;
mod rkisp_worker;
mod soft_worker;
mod still_process;
mod still_runner;

pub(crate) use af_controls::metas as af_metas;
pub(crate) use crop_control::metas as crop_metas;
pub(crate) use loop_controls::LoopControls;
pub(crate) use still_runner::StillJob;

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use smallvec::{SmallVec, smallvec};
use styx_capture::prelude::*;
use styx_core::prelude::{BackendFrameMeta, ClockConversion, ExternalBacking, NativeFrameMeta};
use styx_native::{CameraInfo, NativeCamera, StreamSettings};
use styx_pipeline::SensorValues;
use styx_pipeline::device::{
    IspKind, PispOptions, PispPipeline, SoftPipeline, find_tuning_with, isp_kind,
    soft_capture_memory,
};
use styx_pisp::device::OutputMemory;
use styx_softisp::Scale;

use self::output_buffers::{MAX_OUTPUT_BUFFERS, cma_free, pisp_output_buffers};
use super::control_plane::ControlPlane;
use super::handle::{CaptureHandle, CaptureQueue, WorkerHandle};
use super::request::CaptureError;
use super::tunables::StyxConfig;
use crate::BackendKind;
use crate::metrics::{AaaSample, CaptureMetrics, StageMetrics};

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
                buffers: config.backends.native.raw_buffers.max(2),
                ..Default::default()
            };
            match NativeCamera::open(info, options) {
                Ok(c) => Ok((c, true)),
                Err(e) => {
                    crate::trace::warn!(backend = "native", error = %e, "cached capture buffers unavailable");
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

    fn host_spans(&self, span: &mut dyn FnMut(usize, usize)) -> bool {
        let data = self.data.as_deref();
        data.map(|b| span(b.as_ptr() as usize, b.len())).is_some()
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

/// What the 3A loop made of frame `seq`, for the capture's metrics; with a focus lens, whether
/// it had settled during the frame (from `controls`).
fn aaa_sample(
    params: &styx_pipeline::styx_algo::Params,
    controls: &styx_native::CameraControls,
    seq: u64,
) -> AaaSample {
    use crate::metrics::{AfModeKind, AfSample, AfStateKind};
    use styx_pipeline::styx_algo::{AfMode, AfState};
    let af = &params.af;
    AaaSample {
        ae_locked: params.ae.locked,
        awb_converged: params.awb.converged,
        colour_temperature: params.colour_temperature,
        lux: params.lux,
        flicker_period: params.ae.flicker_detected,
        // Only cameras with a lens: the lookup takes the control schedule's lock.
        af: af.active.then(|| AfSample {
            state: match af.state {
                AfState::Idle => AfStateKind::Idle,
                AfState::Scanning => AfStateKind::Scanning,
                AfState::Focused => AfStateKind::Focused,
                AfState::Failed => AfStateKind::Failed,
            },
            mode: match af.mode {
                AfMode::Manual => AfModeKind::Manual,
                AfMode::Auto => AfModeKind::Auto,
                AfMode::Continuous => AfModeKind::Continuous,
            },
            lens_dioptres: af.lens_position,
            lens_settled: controls
                .applied(seq)
                .and_then(|c| c.lens)
                .map(|l| l.settled),
        }),
    }
}

/// What the 3A loop knew of frame `seq` for its metadata ([`NativeFrameMeta::aaa`]): its
/// metrics sample with the white balance gains `wb` the ISP applied.
fn native_aaa(s: &AaaSample, wb: [f64; 3]) -> styx_core::buffer::NativeAaa {
    use crate::metrics::AfStateKind;
    use styx_core::buffer::{NativeAaa, NativeAfState, NativeLens};
    NativeAaa {
        colour_gains: wb.map(|g| g as f32),
        colour_temperature_k: s.colour_temperature as f32,
        lux: s.lux as f32,
        ae_locked: s.ae_locked,
        awb_converged: s.awb_converged,
        flicker_period_ns: s.flicker_period.map_or(0, |d| d.as_nanos() as u64),
        lens: s.af.map(|af| NativeLens {
            position_dioptres: af.lens_dioptres.unwrap_or(0.0) as f32,
            af_state: match af.state {
                AfStateKind::Idle => NativeAfState::Idle,
                AfStateKind::Scanning => NativeAfState::Scanning,
                AfStateKind::Focused => NativeAfState::Focused,
                AfStateKind::Failed => NativeAfState::Failed,
            },
            settled: af.lens_settled,
        }),
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
        frame_end_ns: 0,
        aaa: None,
    }
}

fn frame_meta(
    mode: &Mode,
    sequence: u64,
    timestamp: Duration,
    s: &SensorValues,
    conversion: Option<ClockConversion>,
) -> FrameMeta {
    super::native_backend::stamp_clock(
        FrameMeta::new(mode.format, timestamp.as_nanos() as u64)
            .with_backend(BackendFrameMeta::Native(native_meta(sequence, s)))
            .with_capture_instant(std::time::Instant::now()),
        conversion,
    )
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
        data: {
            styx_core::metrics::copied(styx_core::metrics::CopySite::Raw, len);
            f.raw.data()[..len].to_vec()
        },
        sensor: f.sensor,
        isp: f.output.applied.clone(),
        params: Box::new(f.output.step.params.clone()),
    })
}

/// The media bus code of the sensor mode of `(w, h)` at the configured raw depth
/// (`NativeIspConfig::raw_bits`); `None` without one (the first mode of the size).
fn raw_code(
    info: &styx_native::CameraInfo,
    (w, h): (u32, u32),
    cfg: &crate::capture_api::NativeIspConfig,
) -> Result<Option<u32>, CaptureError> {
    let bits = cfg.raw_bits;
    let at_size = |m: &&styx_native::SensorMode| (m.width, m.height) == (w, h);
    let Some(bits) = bits else {
        // The sensor's own preference (its description's `processed_format`), when it has a
        // mode of the size in that format; else the first mode.
        let preferred = info.description.sensor.processed_format.as_deref();
        return Ok(preferred.and_then(|f| {
            info.modes
                .iter()
                .filter(at_size)
                .find(|m| m.format == f)
                .map(|m| m.code)
        }));
    };
    info.modes
        .iter()
        .filter(at_size)
        .find(|m| m.bits == bits)
        .map(|m| Some(m.code))
        .ok_or_else(|| {
            err(format!(
                "{} has no {bits}-bit {w}x{h} mode",
                info.location.sensor_name
            ))
        })
}

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
    let (tuning, source) =
        find_tuning_with(&camera.info().description, config.tuning_overlay.as_deref());
    let kind = isp_kind(camera.info());
    crate::trace::info!(backend = "native", isp = kind.name(), tuning = %source, "processed capture");
    let capture = config.capture_tunables();
    let timestamp_clock = capture.timestamp_clock;
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
    loop_controls.af.set_output(w, h);
    loop_controls.af.set_lens(camera.info().lens.is_some());
    // The PiSP's main output at the mode's size can be a crop of the frame (`OUTPUT_CROP`).
    if kind == IspKind::Pisp
        && let Ok((specs, _)) = pisp_worker::output_specs(&mode, &config.backends.native)
        && specs[0].is_some_and(|s| (s.width, s.height) == (w, h))
    {
        loop_controls
            .crop
            .enable((w, h), config.backends.native.crop);
    }
    // The software ISP processes a crop alone, at full resolution, on a full-size mode, and
    // the regions (`NativeIspConfig::regions`) in the same pass, in the frame's format.
    if kind == IspKind::Software && soft_capture_size(camera.info(), (w, h)).1 == Scale::Full {
        loop_controls
            .crop
            .enable((w, h), config.backends.native.crop);
        for (k, region) in config.backends.native.regions.iter().enumerate() {
            let (Some(region), Some(control)) = (region, loop_controls.regions.slot(k)) else {
                continue;
            };
            if region.format.is_some_and(|f| f != mode.format.code) {
                return Err(err(format!(
                    "region {}: the software ISP makes regions in the frame's format ({})",
                    k + 1,
                    mode.format.code
                )));
            }
            control.enable((w, h), region.rect);
        }
    }
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
    let still = still_runner::StillRunner::new(
        Arc::clone(&loop_controls),
        Box::new(move || still_process::StillProcessor::spawn(Arc::clone(&still_ctx))),
    );
    let live = CaptureMetrics::default();
    live.set_isp(kind.name());
    let live_worker = live.clone();
    let (controls, worker): (styx_native::CameraControls, thread::JoinHandle<()>) = match kind {
        IspKind::Pisp => {
            let settings = StreamSettings {
                width: w,
                height: h,
                fourcc: None,
                code: raw_code(camera.info(), (w, h), &config.backends.native)?,
                interval: fraction,
                hflip: config.backends.native.hflip,
                vflip: config.backends.native.vflip,
            };
            let (specs, second_kind) = pisp_worker::output_specs(&mode, &config.backends.native)?;
            let main = specs[0].ok_or_else(|| err("no main output"))?;
            let regions = pisp_regions::Regions::new(
                &config.backends.native,
                main,
                (w, h),
                &loop_controls.regions,
            )?;
            let setup = |i: usize| specs[i].map(|s| s.setup()).transpose();
            let (be_buffers, contiguous) = pisp_output_buffers(
                config,
                specs.iter().flatten().map(|s| s.bytes()).sum(),
                cma_free().filter(|_| !config.backends.native.driver_buffers),
            );
            // Extra passes write into the main output's buffers: half as many again per pass
            // (at least 3: one being written, one queued, one read), within the limit (a pass
            // without a free buffer skips a frame; buffers beyond the contiguous memory come
            // from the system heap).
            let passes = pisp_regions::pass_count(&config.backends.native) as u32;
            let pass_buffers = (passes * (be_buffers / 2).max(3))
                .min(MAX_OUTPUT_BUFFERS.saturating_sub(be_buffers));
            let options = PispOptions {
                outputs: [setup(0)?, setup(1)?],
                output_memory: if config.backends.native.driver_buffers {
                    OutputMemory::Driver
                } else {
                    OutputMemory::CachedHeapContiguousUpTo(contiguous)
                },
                temporal_denoise: config.backends.native.temporal_denoise,
                tdn_compressed: config.backends.native.tdn_compressed,
                slice_line: config.backends.native.slice_line,
                spatial_denoise: f64::from(config.backends.native.spatial_denoise_percent) / 100.0,
                be_buffers,
                // The base set now, the rest as they are held.
                be_buffers_initial: Some(
                    config.backends.native.output_buffers.clamp(2, be_buffers),
                ),
                pass_buffers,
                raw_lease: config.backends.native.sensor_raw,
                fe_buffers: PispOptions::nv12_and_half_rgb(w, h).fe_buffers
                    + if config.backends.native.sensor_raw {
                        PispOptions::RAW_LEASE_BUFFERS
                    } else {
                        0
                    },
                ..PispOptions::nv12_and_half_rgb(w, h)
            };
            let mut p = PispPipeline::open(camera, &settings, &tuning, options).map_err(err)?;
            // Temporal denoise was asked for but the device refused its buffers: the frames
            // run without it (spatial denoise at its no-TDN values), so say so.
            let (temporal, tdn_error) = p.temporal_denoise();
            match tdn_error {
                Some(why) => crate::trace::warn!(
                    backend = "native",
                    error = %why,
                    "temporal denoise unavailable; frames run without it"
                ),
                None => crate::trace::info!(
                    backend = "native",
                    temporal_denoise = temporal,
                    "processed capture: temporal denoise state"
                ),
            }
            if let Some(why) = p.slice_error() {
                crate::trace::warn!(backend = "native", error = %why, "frames delivered whole");
            }
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
                    crop: None,
                    regions,
                    tx,
                    owns_queue,
                    stop: stop_rx,
                    error: werr,
                    send_timeout,
                    timeout,
                    loop_controls: loop_worker,
                    still,
                    live: live_worker,
                    clock: timestamp_clock,
                    prep: pisp_lease::CpuPrep::new(&config.backends.native),
                    slices: config.backends.native.deliver_slices,
                },
            )?;
            (controls, worker)
        }
        IspKind::Rkisp => rkisp_worker::open(
            (
                raw_code(camera.info(), (w, h), &config.backends.native)?,
                camera,
            ),
            (fraction, &tuning, &loop_controls),
            config,
            rkisp_worker::RkispWorker {
                mode: worker_mode,
                keep: config.backends.native.output_buffers.max(2) as usize,
                tx,
                owns_queue,
                stop: stop_rx,
                error: werr,
                send_timeout,
                timeout,
                loop_controls: loop_worker,
                still,
                live: live_worker,
                clock: timestamp_clock,
                controls: None,
            },
        )?,
        IspKind::Software => {
            let ((cw, ch), scale) = soft_capture_size(camera.info(), (w, h));
            let settings = StreamSettings {
                width: cw,
                height: ch,
                fourcc: None,
                code: None,
                interval: fraction,
                hflip: config.backends.native.hflip,
                vflip: config.backends.native.vflip,
            };
            let threads = config
                .backends
                .native
                .soft_threads
                .unwrap_or_else(crate::planner::cost::default_softisp_threads);
            crate::trace::info!(backend = "native", threads, "software ISP threads");
            let mut p = SoftPipeline::open(camera, &settings, &tuning, threads).map_err(err)?;
            p.soft_loop()
                .set_post_stages(config.backends.native.soft_post_stages);
            #[cfg(feature = "gpu-isp")]
            if let Some(ctx) = crate::gpu_isp::context() {
                match p.use_gpu(&ctx) {
                    Ok(()) => {
                        live.set_isp("gpu");
                        crate::trace::info!(backend = "native", device = %ctx.info().name, "GPU ISP")
                    }
                    Err(e) => {
                        crate::trace::warn!(backend = "native", error = %e, "GPU ISP refused; software ISP")
                    }
                }
            }
            if let Some(c) = loop_controls.take() {
                p.soft_loop().controller().set_controls(c);
            }
            p.start().map_err(err)?;
            let controls = p.controls().clone();
            // Regions need a full-size mode to crop (the CPU or the GPU ISP); the binned
            // overview needs the CPU ISP.
            let cpu = matches!(p.soft_loop().engine(), styx_pipeline::IspEngine::Cpu { .. });
            let overview = config
                .backends
                .native
                .overview
                .filter(|_| cpu)
                .map(|wanted| crate::planner::soft_overview_factor((cw, ch), wanted));
            let worker = soft_worker::spawn(
                p,
                soft_worker::SoftWorker {
                    mode: worker_mode,
                    keep: config.backends.native.output_buffers.max(2) as usize,
                    scale,
                    crops: scale == Scale::Full,
                    overview: overview.filter(|_| scale == Scale::Full),
                    tx,
                    owns_queue,
                    stop: stop_rx,
                    error: werr,
                    send_timeout,
                    timeout,
                    loop_controls: loop_worker,
                    still,
                    live: live_worker,
                    clock: timestamp_clock,
                    controls: controls.clone(),
                    sensor_raw: config.backends.native.sensor_raw,
                },
            )?;
            (controls, worker)
        }
        other => return Err(err(format!("no capture path for the {} ISP", other.name()))),
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
        sequence_gaps: live.sequence_gaps(),
        live,
    })
}

#[cfg(test)]
mod tests;
