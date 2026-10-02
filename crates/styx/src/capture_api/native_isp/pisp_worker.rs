//! The PiSP capture worker: frames leased straight from the back end's output buffers, the
//! second output attached to each frame as a `CompanionKind::Scaled` companion.
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
use styx_capture::prelude::*;
use styx_core::prelude::{
    BackendFrameMeta, CompanionKind, ExternalBacking, FrameBackingExport, FrameExportError,
    FrameFdPlane, FrameResidency, TimestampClock,
};
use styx_core::queue::BoundedTx;
use styx_kernel::Mapping;
use styx_kernel::dma_heap::{self, Access, DmaBuf};
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

    pub(super) fn setup(self) -> Result<BeOutputSetup, CaptureError> {
        Ok(BeOutputSetup {
            format: self.be_format()?,
            width: self.width,
            height: self.height,
        })
    }
}

/// The outputs a processed `mode` capture delivers with `cfg`: the main one (the mode's
/// format and size unless `cfg` says otherwise) and the optional second one.
pub(super) fn output_specs(
    mode: &Mode,
    cfg: &NativeIspConfig,
) -> Result<[Option<OutputSpec>; 2], CaptureError> {
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
    let second = cfg.second_output.map(|((width, height), code)| OutputSpec {
        code,
        width,
        height,
    });
    if let Some(s) = second {
        s.be_format()?;
    }
    Ok([Some(main), second])
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
    output: usize,
    index: u32,
    synced: AtomicBool,
    returns: mpsc::Sender<(usize, u32)>,
}

impl ExternalBacking for BeBacking {
    fn plane_data(&self, _index: usize) -> Option<&[u8]> {
        // The pixels are about to be read on the CPU: sync once (a no-op for the driver's
        // uncached buffers, cache maintenance for cached ones).
        if !self.synced.swap(true, Ordering::AcqRel) {
            let _ = dma_heap::sync(self.buffer.fd.as_fd(), Access::Read, true);
        }
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
    pub(super) strides: [usize; 2],
    pub(super) tx: BoundedTx<FrameLease>,
    pub(super) stop: mpsc::Receiver<()>,
    pub(super) error: Arc<Mutex<Option<CaptureError>>>,
    pub(super) send_timeout: Duration,
    pub(super) timeout: Duration,
}

fn lease(
    spec: OutputSpec,
    stride: usize,
    buffer: BeBuffer,
    (output, index): (usize, u32),
    f: &PispFrame,
    returns: &mpsc::Sender<(usize, u32)>,
) -> FrameLease {
    let plane_layouts = layouts(spec.code, spec.height as usize, stride);
    let len = plane_layouts
        .iter()
        .map(|l| l.offset + l.len)
        .max()
        .unwrap_or(0);
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
            len,
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
            loop {
                if w.stop.try_recv().is_ok() {
                    break;
                }
                while let Ok((i, index)) = ret_rx.try_recv() {
                    p.release_output(i, index);
                }
                let f = match p.next(w.timeout) {
                    Ok(f) => f,
                    Err(e) => {
                        *w.error.lock() = Some(err(e));
                        break;
                    }
                };
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
                    Some(s) => match main.with_companion(CompanionKind::Scaled, s) {
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
            tx.close();
        })
        .map_err(|e| err(format!("worker: {e}")))
}
