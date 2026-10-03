//! The PiSP capture worker: frames leased straight from the back end's output buffers, the
//! second output attached to each frame as a `CompanionKind::Scaled` companion, or as a
//! `CompanionKind::Pyramid` one (the main output scaled by `2^-level`).
//!
//! Each output buffer is mapped and exported once (cached per buffer index) and shared by the
//! leases that hold it; a lease returns its buffer to the back end when it drops, so the two
//! outputs of a job can live for different times (consumers on a shared capture take the
//! output they want). The CPU-access sync (`DMA_BUF_IOCTL_SYNC`) happens only when someone
//! reads the pixels, not for frames handed on as dma-bufs (encoders, GPUs, other processes).

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use smallvec::SmallVec;
use styx_capture::prelude::*;
use styx_core::prelude::{
    BackendFrameMeta, CompanionKind, ExternalBacking, FrameBackingExport, FrameExportError,
    FrameFdPlane, FrameResidency, TimestampClock,
};
use styx_core::queue::BoundedTx;
use styx_kernel::Mapping;
use styx_kernel::dma_heap::{self, Access, DmaBuf};
use styx_pipeline::PipelineError;
use styx_pipeline::device::{PispFrame, PispPipeline};
use styx_pisp::device::{BeFormat, BeOutputSetup};

use super::super::handle::enqueue_capture_frame;
use super::super::request::CaptureError;
use super::super::tunables::NativeIspConfig;
use super::{err, layouts, native_meta};

/// One back end output as the capture delivers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OutputSpec {
    pub(super) code: FourCc,
    pub(super) width: u32,
    pub(super) height: u32,
}

impl OutputSpec {
    fn be_format(self) -> Result<BeFormat, CaptureError> {
        match self.code {
            FourCc::NV12 => Ok(BeFormat::Nv12),
            FourCc::RG24 => Ok(BeFormat::Rgb24),
            c => Err(err(format!("the PiSP back end does not make {c}"))),
        }
    }

    /// Bytes of one buffer of this output.
    pub(super) fn bytes(self) -> u64 {
        let px = u64::from(self.width) * u64::from(self.height);
        if self.code == FourCc::NV12 {
            px * 3 / 2
        } else {
            px * 3
        }
    }

    pub(super) fn setup(self) -> Result<BeOutputSetup, CaptureError> {
        Ok(BeOutputSetup {
            format: self.be_format()?,
            width: self.width,
            height: self.height,
        })
    }
}

/// The outputs a processed `mode` capture delivers with `cfg`: the main one (the mode's
/// format and size unless `cfg` says otherwise) and the optional second one, with the kind of
/// companion it is attached as (a pyramid level of the main output, else another size).
pub(super) fn output_specs(
    mode: &Mode,
    cfg: &NativeIspConfig,
) -> Result<([Option<OutputSpec>; 2], CompanionKind), CaptureError> {
    let res = mode.format.resolution;
    let (width, height) = cfg
        .output_size
        .unwrap_or((res.width.get(), res.height.get()));
    let main = OutputSpec {
        code: cfg.output_format.unwrap_or(mode.format.code),
        width,
        height,
    };
    main.be_format()?;
    let level = cfg.pyramid_level.min(3);
    if level > 0 {
        if cfg.second_output.is_some() {
            tracing::warn!(
                backend = "native",
                level,
                "second output disabled: the pyramid companion uses it"
            );
        }
        // Even sizes, as the chroma planes need.
        let half = |v: u32| ((v >> level) & !1).max(2);
        let pyramid = OutputSpec {
            code: main.code,
            width: half(width),
            height: half(height),
        };
        return Ok((
            [Some(main), Some(pyramid)],
            CompanionKind::Pyramid { level },
        ));
    }
    let second = cfg.second_output.map(|((width, height), code)| OutputSpec {
        code,
        width,
        height,
    });
    if let Some(s) = second {
        s.be_format()?;
    }
    Ok(([Some(main), second], CompanionKind::Scaled))
}

/// A back end output buffer, mapped once and shared by the leases of the frames it holds.
#[derive(Clone)]
struct BeBuffer {
    map: Arc<Mapping>,
    fd: Arc<OwnedFd>,
}

struct BeBacking {
    buffer: BeBuffer,
    /// Each plane's `(offset, len)` in the buffer (plane views start at their plane, as
    /// for other dma-buf backings, so exports carry real plane offsets).
    planes: SmallVec<[(usize, usize); 3]>,
    output: usize,
    index: u32,
    synced: AtomicBool,
    returns: mpsc::Sender<(usize, u32)>,
}

impl ExternalBacking for BeBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        let &(offset, len) = self.planes.get(index)?;
        // The pixels are about to be read on the CPU: sync once (a no-op for the driver's
        // uncached buffers, cache maintenance for cached ones).
        if !self.synced.swap(true, Ordering::AcqRel) {
            let _ = dma_heap::sync(self.buffer.fd.as_fd(), Access::Read, true);
        }
        self.buffer.map.as_slice().get(offset..offset + len)
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
        let planes = self
            .planes
            .iter()
            .map(|&(offset, len)| {
                Ok(FrameFdPlane {
                    fd: self
                        .buffer
                        .fd
                        .as_fd()
                        .try_clone_to_owned()
                        .map_err(FrameExportError::Fd)?,
                    offset,
                    len,
                })
            })
            .collect::<Result<Vec<_>, FrameExportError>>()?;
        Ok(Some(FrameBackingExport::DmabufPlanes { planes }))
    }
}

impl Drop for BeBacking {
    fn drop(&mut self) {
        if self.synced.load(Ordering::Acquire) {
            let _ = dma_heap::sync(self.buffer.fd.as_fd(), Access::Read, false);
        }
        let _ = self.returns.send((self.output, self.index));
    }
}

/// Output buffers by (output, index), mapped and exported on first use.
struct Buffers {
    cache: HashMap<(usize, u32), BeBuffer>,
}

impl Buffers {
    fn get(&mut self, p: &PispPipeline, f: &PispFrame, i: usize) -> Result<BeBuffer, String> {
        let index = f.job.outputs[i].ok_or("no buffer")?;
        if let Some(b) = self.cache.get(&(i, index)) {
            return Ok(b.clone());
        }
        let fd = p
            .output_dmabuf(i, &f.job)
            .and_then(|fd| fd.try_clone_to_owned().ok())
            .ok_or("no dma-buf")?;
        let size = p.output(i, &f.job).map_or(0, <[u8]>::len);
        let dup = fd.try_clone().map_err(|e| e.to_string())?;
        let map = DmaBuf::from_fd(dup, size)
            .map()
            .map_err(|e| e.to_string())?;
        let b = BeBuffer {
            map: Arc::new(map),
            fd: Arc::new(fd),
        };
        self.cache.insert((i, index), b.clone());
        Ok(b)
    }
}

/// What the worker needs besides the pipeline.
pub(super) struct Worker {
    pub(super) specs: [Option<OutputSpec>; 2],
    /// What the second output is attached as.
    pub(super) second_kind: CompanionKind,
    pub(super) strides: [usize; 2],
    pub(super) tx: BoundedTx<FrameLease>,
    /// Whether `tx` was made for this capture (closed when it ends) or is the consumer's.
    pub(super) owns_queue: bool,
    pub(super) stop: mpsc::Receiver<()>,
    pub(super) error: Arc<Mutex<Option<CaptureError>>>,
    pub(super) send_timeout: Duration,
    pub(super) timeout: Duration,
    /// The 3A loop's controls and state.
    pub(super) loop_controls: Arc<super::LoopControls>,
}

fn lease(
    spec: OutputSpec,
    stride: usize,
    buffer: BeBuffer,
    (output, index): (usize, u32),
    f: &PispFrame,
    returns: &mpsc::Sender<(usize, u32)>,
) -> FrameLease {
    let in_buffer = layouts(spec.code, spec.height as usize, stride);
    let planes = in_buffer.iter().map(|l| (l.offset, l.len)).collect();
    let plane_layouts = in_buffer
        .iter()
        .map(|l| PlaneLayout { offset: 0, ..*l })
        .collect();
    let res = Resolution::new(spec.width, spec.height).expect("non-zero output size");
    let format = MediaFormat::new(spec.code, res, ColorSpace::Srgb);
    let mut meta = FrameMeta::new(format, f.timestamp.as_nanos() as u64)
        .with_backend(BackendFrameMeta::Native(native_meta(f.sequence, &f.sensor)))
        .with_capture_instant(std::time::Instant::now());
    meta.clock = Some(TimestampClock::Monotonic);
    FrameLease::from_external(
        meta,
        plane_layouts,
        Arc::new(BeBacking {
            buffer,
            planes,
            output,
            index,
            synced: AtomicBool::new(false),
            returns: returns.clone(),
        }),
    )
}

/// Runs the capture on its own thread until stopped or the queue closes.
pub(super) fn spawn(
    mut p: PispPipeline,
    w: Worker,
) -> Result<thread::JoinHandle<()>, CaptureError> {
    thread::Builder::new()
        .name("styx-native-pisp".into())
        .spawn(move || {
            let tx = &w.tx;
            let (ret_tx, ret_rx) = mpsc::channel::<(usize, u32)>();
            let mut buffers = Buffers {
                cache: HashMap::new(),
            };
            let mut held_drops = 0u64;
            loop {
                if w.stop.try_recv().is_ok() {
                    break;
                }
                while let Ok((i, index)) = ret_rx.try_recv() {
                    p.release_output(i, index);
                }
                if let Some(c) = w.loop_controls.take() {
                    p.controller().set_controls(c);
                }
                let f = match p.next(w.timeout) {
                    Ok(f) => f,
                    // Consumers hold every output buffer: this frame is dropped (never wait
                    // for them: the camera keeps running), the next one after a buffer comes
                    // back is processed.
                    Err(PipelineError::OutputsHeld(output)) => {
                        held_drops += 1;
                        if held_drops == 1 {
                            tracing::info!(
                                backend = "native",
                                output,
                                "consumers hold every back end buffer: dropping frames until one is released"
                            );
                        } else {
                            tracing::debug!(backend = "native", output, held_drops, "frame dropped: buffers held");
                        }
                        continue;
                    }
                    Err(e) => {
                        *w.error.lock() = Some(err(e));
                        break;
                    }
                };
                w.loop_controls.report(&p.step().params);
                let mut leases: [Option<FrameLease>; 2] = [None, None];
                let mut failed = None;
                for (i, spec) in w.specs.iter().enumerate() {
                    let (Some(spec), Some(index)) = (spec, f.job.outputs[i]) else {
                        continue;
                    };
                    match buffers.get(&p, &f, i) {
                        Ok(b) => {
                            leases[i] =
                                Some(lease(*spec, w.strides[i], b, (i, index), &f, &ret_tx));
                        }
                        Err(e) => {
                            p.release_output(i, index);
                            failed = Some(e);
                        }
                    }
                }
                if let Some(e) = failed {
                    *w.error.lock() = Some(err(format!("output buffer: {e}")));
                    break;
                }
                let [Some(main), second] = leases else {
                    continue;
                };
                let frame = match second {
                    Some(s) => match main.with_companion(w.second_kind, s) {
                        Ok(f) => f,
                        Err(e) => {
                            *w.error.lock() = Some(err(e));
                            break;
                        }
                    },
                    None => main,
                };
                if enqueue_capture_frame(tx, frame, "native-pisp", w.send_timeout) {
                    break;
                }
            }
            // Leases still out return their buffers to a closed channel; the back end frees
            // its buffers once they are dropped.
            if let Err(e) = p.close() {
                tracing::warn!(backend = "native", error = %e, "closing the PiSP path");
            }
            if w.owns_queue {
                tx.close();
            }
        })
        .map_err(|e| err(format!("worker: {e}")))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use styx_pipeline::SensorValues;
    use styx_pipeline::device::PispTimes;
    use styx_pisp::device::BeJob;

    use super::*;

    /// A back end buffer stand-in: a memfd holding an NV12 frame (Y = 1, CbCr = 2).
    fn buffer(w: usize, h: usize) -> BeBuffer {
        let len = w * h * 3 / 2;
        let buf = DmaBuf::memfd("nv12", len).unwrap();
        let mut map = buf.map().unwrap();
        map.as_mut_slice()[..w * h].fill(1);
        map.as_mut_slice()[w * h..].fill(2);
        BeBuffer {
            map: Arc::new(map),
            fd: Arc::new(buf.into_fd()),
        }
    }

    fn frame() -> PispFrame {
        PispFrame {
            sequence: 7,
            timestamp: Duration::from_millis(5),
            dequeued: Instant::now(),
            sensor: SensorValues {
                frame: 7,
                exposure: Duration::from_millis(10),
                analogue_gain: 2.0,
                digital_gain: 1.0,
                frame_duration: Duration::from_millis(33),
                verified: true,
            },
            job: BeJob {
                outputs: [Some(3), Some(1)],
                elapsed: Duration::ZERO,
            },
            sequence_mismatch: false,
            request_lands: None,
            settings_from: Some(6),
            digital_gain: 1.0,
            flicker: 1.0,
            times: PispTimes::default(),
        }
    }

    #[test]
    fn nv12_leases_export_their_planes_and_return_the_buffer() {
        let (w, h) = (64, 32);
        let spec = OutputSpec {
            code: FourCc::NV12,
            width: w as u32,
            height: h as u32,
        };
        let (tx, rx) = mpsc::channel();
        let f = lease(spec, w, buffer(w, h), (0, 3), &frame(), &tx);
        let planes = f.planes();
        assert_eq!(planes.len(), 2);
        assert!(planes[0].data().iter().all(|&v| v == 1));
        assert_eq!(planes[1].data().len(), w * h / 2);
        assert!(planes[1].data().iter().all(|&v| v == 2));
        assert_eq!(f.residency(), FrameResidency::Dmabuf);
        // Exported as one plane per layout with its offset in the buffer, as another process
        // (or a GPU) imports it.
        let FrameBackingExport::DmabufPlanes { planes: fds } = f.export_backing().unwrap() else {
            panic!("not a dma-buf export");
        };
        assert_eq!(
            fds.iter().map(|p| (p.offset, p.len)).collect::<Vec<_>>(),
            [(0, w * h), (w * h, w * h / 2)]
        );
        let imported = FrameLease::from_dmabuf(f.meta().clone(), f.layouts(), fds).expect("import");
        assert!(imported.planes()[1].data().iter().all(|&v| v == 2));
        drop(imported);
        assert!(rx.try_recv().is_err(), "returned while held");
        drop(planes);
        drop(f);
        assert_eq!(rx.try_recv().unwrap(), (0, 3));
    }

    #[test]
    fn output_specs_follow_the_config() {
        let res = Resolution::new(1280, 800).unwrap();
        let format = MediaFormat::new(FourCc::NV12, res, ColorSpace::Srgb);
        let mode = Mode {
            id: ModeId {
                format,
                interval: None,
            },
            format,
            intervals: Default::default(),
            interval_stepwise: None,
        };
        let (specs, kind) = output_specs(&mode, &NativeIspConfig::default()).unwrap();
        assert_eq!(specs[0].unwrap().code, FourCc::NV12);
        assert!(specs[1].is_none());
        assert_eq!(kind, CompanionKind::Scaled);
        let cfg = NativeIspConfig {
            output_size: Some((640, 400)),
            output_format: Some(FourCc::RG24),
            second_output: Some(((320, 200), FourCc::NV12)),
            ..Default::default()
        };
        let (specs, _) = output_specs(&mode, &cfg).unwrap();
        assert_eq!(
            (specs[0].unwrap().width, specs[0].unwrap().code),
            (640, FourCc::RG24)
        );
        assert_eq!(specs[1].unwrap().height, 200);
        let bad = NativeIspConfig {
            output_format: Some(FourCc::YUYV),
            ..Default::default()
        };
        assert!(output_specs(&mode, &bad).is_err());
        // A pyramid level takes the second output: the main output halved, in its format.
        let pyramid = NativeIspConfig {
            pyramid_level: 1,
            second_output: Some(((320, 200), FourCc::RG24)),
            ..Default::default()
        };
        let (specs, kind) = output_specs(&mode, &pyramid).unwrap();
        assert_eq!(kind, CompanionKind::Pyramid { level: 1 });
        let s = specs[1].unwrap();
        assert_eq!((s.width, s.height, s.code), (640, 400, FourCc::NV12));
        let quarter = NativeIspConfig {
            output_size: Some((1280, 720)),
            pyramid_level: 2,
            ..Default::default()
        };
        let (specs, kind) = output_specs(&mode, &quarter).unwrap();
        assert_eq!(kind, CompanionKind::Pyramid { level: 2 });
        assert_eq!(
            (specs[1].unwrap().width, specs[1].unwrap().height),
            (320, 180)
        );
    }
}
