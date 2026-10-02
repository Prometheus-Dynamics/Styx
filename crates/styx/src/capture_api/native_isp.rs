//! Processed frames from native cameras: the 3A loop of `styx-pipeline` closed over the
//! camera, with the PiSP (front end statistics, back end NV12/RGB, raw frames handed over as
//! dma-bufs) where the camera's receiver has one, else the software ISP.
//!
//! The native backend lists `NV12` and `RG24` modes next to the raw ones; the planner prices
//! them by ISP (see `planner::cost`) and this module runs them. PiSP frames are leased straight
//! from the back end's output buffers (mapped once, exported as dma-bufs, returned to the back
//! end when the lease drops); software ISP frames are written into recycled heap buffers.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use smallvec::{SmallVec, smallvec};
use styx_capture::prelude::*;
use styx_core::prelude::{
    BackendFrameMeta, ExternalBacking, FrameBackingExport, FrameExportError, FrameFdPlane,
    FrameResidency, NativeFrameMeta, TimestampClock,
};
use styx_kernel::Mapping;
use styx_kernel::dma_heap::{self, Access, DmaBuf};
use styx_native::{CameraInfo, NativeCamera, StreamSettings};
use styx_pipeline::SensorValues;
use styx_pipeline::device::{
    IspKind, PispOptions, PispPipeline, SoftPipeline, find_tuning, isp_kind,
};
use styx_pisp::device::{BeFormat, BeJob, BeOutputSetup};
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

/// `NV12` and `RG24` modes at each size the sensor's raw modes have, with their intervals.
pub(crate) fn processed_modes(raw: &[Mode]) -> Vec<Mode> {
    let mut out: Vec<Mode> = Vec::new();
    for m in raw {
        let res = m.format.resolution;
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
    out
}

fn err(e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Backend(format!("native ISP: {e}"))
}

/// A back end output buffer, mapped once and shared by the leases of the frames it holds.
#[derive(Clone)]
struct BeBuffer {
    map: Arc<Mapping>,
    fd: Arc<OwnedFd>,
}

struct BeBacking {
    buffer: BeBuffer,
    len: usize,
    job: BeJob,
    returns: mpsc::Sender<BeJob>,
}

impl ExternalBacking for BeBacking {
    fn plane_data(&self, _index: usize) -> Option<&[u8]> {
        let d = self.buffer.map.as_slice();
        Some(&d[..self.len.min(d.len())])
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.buffer.map.len())
    }

    fn backing_kind(&self) -> &'static str {
        "pispbe_dmabuf"
    }

    fn can_export(&self) -> bool {
        true
    }

    fn residency(&self) -> FrameResidency {
        FrameResidency::Dmabuf
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        use std::os::fd::AsFd;
        let fd = self
            .buffer
            .fd
            .as_fd()
            .try_clone_to_owned()
            .map_err(FrameExportError::Fd)?;
        Ok(Some(FrameBackingExport::DmabufPlanes {
            planes: vec![FrameFdPlane {
                fd,
                offset: 0,
                len: self.len,
            }],
        }))
    }
}

impl Drop for BeBacking {
    fn drop(&mut self) {
        use std::os::fd::AsFd;
        let _ = dma_heap::sync(self.buffer.fd.as_fd(), Access::Read, false);
        let _ = self.returns.send(self.job);
    }
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

fn frame_meta(mode: &Mode, sequence: u64, timestamp: Duration, s: &SensorValues) -> FrameMeta {
    let native = NativeFrameMeta {
        sequence: sequence as u32,
        bytes_used: 0,
        error: false,
        exposure_ns: s.exposure.as_nanos() as u64,
        analog_gain: s.analogue_gain as f32,
        digital_gain: s.digital_gain as f32,
        frame_duration_ns: s.frame_duration.as_nanos() as u64,
        frame_length: 0,
        verified: s.verified,
    };
    let mut meta = FrameMeta::new(mode.format, timestamp.as_nanos() as u64)
        .with_backend(BackendFrameMeta::Native(native))
        .with_capture_instant(std::time::Instant::now());
    meta.clock = Some(TimestampClock::Monotonic);
    meta
}

/// Starts processed capture on an opened camera.
#[allow(clippy::too_many_arguments)]
pub(super) fn start_processed(
    camera: NativeCamera,
    mode: Mode,
    interval: Option<Interval>,
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
    let (tx, rx) = queue.unwrap_or_else(|| {
        styx_core::queue::bounded_with(capture.queue_depth.max(1), capture.queue_overflow)
    });
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let worker_error = Arc::new(Mutex::new(None));
    let werr = Arc::clone(&worker_error);
    let send_timeout = Duration::from_millis(capture.queue_send_timeout_ms);
    let timeout = Duration::from_secs(2);
    let code = mode.format.code;
    let worker_mode = mode.clone();
    let (controls, worker): (styx_native::CameraControls, thread::JoinHandle<()>) = match kind {
        IspKind::Pisp => {
            let settings = StreamSettings {
                width: w,
                height: h,
                fourcc: None,
                code: None,
                interval: fraction,
            };
            let format = if code == FourCc::NV12 {
                BeFormat::Nv12
            } else {
                BeFormat::Rgb24
            };
            let options = PispOptions {
                outputs: [
                    Some(BeOutputSetup {
                        format,
                        width: w,
                        height: h,
                    }),
                    None,
                ],
                ..PispOptions::nv12_and_half_rgb(w, h)
            };
            let mut p = PispPipeline::open(camera, &settings, &tuning, options).map_err(err)?;
            p.start().map_err(err)?;
            let controls = p.controls().clone();
            let of = p.output_format(0).ok_or_else(|| err("no output 0"))?;
            let stride = of.stride as usize;
            let len = layouts(code, h as usize, stride)
                .iter()
                .map(|l| l.offset + l.len)
                .max()
                .unwrap_or(0);
            let worker = thread::Builder::new()
                .name("styx-native-pisp".into())
                .spawn(move || {
                    let (ret_tx, ret_rx) = mpsc::channel::<BeJob>();
                    let mut cache: HashMap<u32, BeBuffer> = HashMap::new();
                    loop {
                        if stop_rx.try_recv().is_ok() {
                            break;
                        }
                        while let Ok(job) = ret_rx.try_recv() {
                            p.release(&job);
                        }
                        let f = match p.next(timeout) {
                            Ok(f) => f,
                            Err(e) => {
                                *werr.lock() = Some(err(e));
                                break;
                            }
                        };
                        let Some(index) = f.job.outputs[0] else {
                            p.release(&f.job);
                            continue;
                        };
                        let buffer = match cache.get(&index) {
                            Some(b) => b.clone(),
                            None => {
                                let Some(fd) = p
                                    .output_dmabuf(0, &f.job)
                                    .and_then(|fd| fd.try_clone_to_owned().ok())
                                else {
                                    p.release(&f.job);
                                    continue;
                                };
                                let size = p.output(0, &f.job).map_or(len, <[u8]>::len);
                                let map = match DmaBuf::from_fd(fd.try_clone().expect("dup"), size)
                                    .map()
                                {
                                    Ok(m) => m,
                                    Err(e) => {
                                        *werr.lock() = Some(err(e));
                                        break;
                                    }
                                };
                                let b = BeBuffer {
                                    map: Arc::new(map),
                                    fd: Arc::new(fd),
                                };
                                cache.insert(index, b.clone());
                                b
                            }
                        };
                        {
                            use std::os::fd::AsFd;
                            let _ = dma_heap::sync(buffer.fd.as_fd(), Access::Read, true);
                        }
                        let meta = frame_meta(&worker_mode, f.sequence, f.timestamp, &f.sensor);
                        let lease = FrameLease::from_external(
                            meta,
                            layouts(code, h as usize, stride),
                            Arc::new(BeBacking {
                                buffer,
                                len,
                                job: f.job,
                                returns: ret_tx.clone(),
                            }),
                        );
                        if enqueue_capture_frame(&tx, lease, "native-pisp", send_timeout) {
                            break;
                        }
                    }
                    // Leases still out return their buffers to a closed channel; the back end
                    // frees its buffers once they are dropped.
                    if let Err(e) = p.close() {
                        tracing::warn!(backend = "native", error = %e, "closing the PiSP path");
                    }
                    tx.close();
                })
                .map_err(|e| err(format!("worker: {e}")))?;
            (controls, worker)
        }
        IspKind::Software => {
            let settings = StreamSettings {
                width: w,
                height: h,
                fourcc: None,
                code: None,
                interval: fraction,
            };
            let mut p = SoftPipeline::open(camera, &settings, &tuning, 1).map_err(err)?;
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
                        let f = match p.next(timeout, Scale::Full, out) {
                            Ok(Some(f)) => f,
                            Ok(None) => break,
                            Err(e) => {
                                *werr.lock() = Some(err(e));
                                break;
                            }
                        };
                        let meta =
                            frame_meta(&worker_mode, f.sensor.frame, f.raw.timestamp, &f.sensor);
                        drop(f);
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
                    tx.close();
                })
                .map_err(|e| err(format!("worker: {e}")))?;
            (controls, worker)
        }
    };
    Ok(CaptureHandle {
        backend: BackendKind::Native,
        control: ControlPlane::Native { controls },
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
        let modes = processed_modes(&[raw(b"pBAA"), raw(b"BA81")]);
        let codes: Vec<String> = modes.iter().map(|m| m.format.code.to_string()).collect();
        assert_eq!(codes, ["NV12", "RG24"]);
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
        modes.extend(processed_modes(&[raw]));
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
        assert!(plan.total.cpu_ms > 8.0, "{plan}");
    }
}
