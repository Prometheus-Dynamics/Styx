//! Frames leased straight from the PiSP back end's output buffers: each buffer is mapped and
//! exported once (cached per buffer index) and shared by the leases that hold it; a lease
//! returns its buffer to the back end when it drops, so the outputs of a job (and its extra
//! passes) can live for different times. The CPU-access sync (`DMA_BUF_IOCTL_SYNC`) happens
//! only when someone reads the pixels, not for frames handed on as dma-bufs.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use smallvec::SmallVec;
use styx_capture::prelude::*;
use styx_core::buffer::CpuReadWindow;
use styx_core::prelude::{
    BackendFrameMeta, CaptureInstant, ClockConversion, ExportedKind, ExternalBacking,
    FrameBackingExport, FrameExportError, FrameFdPlane, FrameRect, FrameResidency, Hop,
};
use styx_kernel::Mapping;
use styx_kernel::dma_heap::{self, Access, DmaBuf};
use styx_pipeline::device::{PispFrame, PispPipeline};
use styx_pisp::device::{BeFormat, BeOutputSetup};
use styx_pisp::uapi::BeCropConfig;

use super::super::native_backend::stamp_clock;
use super::super::request::CaptureError;
use super::{err, layouts, native_meta};
use crate::metrics::CaptureMetrics;

/// Where leases give their output buffers back to the worker: a fixed lock-free ring (sending
/// allocates nothing, unlike a channel that grows in blocks), with room for every buffer.
pub(super) type Returns = styx_core::queue::BoundedTx<(usize, u32)>;

/// The worker's end of [`Returns`].
pub(super) type ReturnsRx = styx_core::queue::BoundedRx<(usize, u32)>;

/// Room for every output buffer of the back end (a few per output, 32 at most each).
const RETURNS: usize = 256;

/// A ring for buffers coming back.
pub(super) fn returns() -> (Returns, ReturnsRx) {
    styx_core::queue::bounded(RETURNS)
}

/// One back end output as the capture delivers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OutputSpec {
    pub(super) code: FourCc,
    pub(super) width: u32,
    pub(super) height: u32,
}

impl OutputSpec {
    pub(super) fn be_format(self) -> Result<BeFormat, CaptureError> {
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

/// A back end output buffer, mapped once and shared by the leases of the frames it holds.
#[derive(Clone)]
pub(super) struct BeBuffer {
    map: Arc<Mapping>,
    fd: Arc<OwnedFd>,
    /// From a cached dma-heap, not the driver's uncached buffers.
    cached: bool,
}

struct BeBacking {
    buffer: BeBuffer,
    /// Each plane's `(offset, len)` in the buffer (plane views start at their plane, as
    /// for other dma-buf backings, so exports carry real plane offsets).
    planes: SmallVec<[(usize, usize); 3]>,
    output: usize,
    index: u32,
    /// CPU access to the buffer (`DMA_BUF_IOCTL_SYNC`): held by plain reads until the lease
    /// drops, or bracketed (Daedalus's `daedalus:frame` access).
    window: CpuReadWindow,
    returns: Returns,
}

impl ExternalBacking for BeBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        let bytes = self.bytes(index)?;
        // The pixels are about to be read on the CPU: sync once (a no-op for the driver's
        // uncached buffers, cache maintenance for cached ones).
        self.window.hold(|| self.sync(true));
        Some(bytes)
    }

    fn begin_cpu_read(&self, index: usize) -> Option<&[u8]> {
        let bytes = self.bytes(index)?;
        self.window.begin(|| self.sync(true));
        Some(bytes)
    }

    fn end_cpu_read(&self, _index: usize) {
        self.window.end(|| self.sync(false));
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

    fn dmabuf_plane(&self, index: usize) -> Option<styx_core::buffer::DmabufPlane<'_>> {
        let &(offset, _) = self.planes.get(index)?;
        Some(styx_core::buffer::DmabufPlane {
            fd: self.buffer.fd.as_fd(),
            offset,
        })
    }

    fn cpu_access(&self) -> CpuAccess {
        if self.buffer.cached {
            CpuAccess::Cached
        } else {
            CpuAccess::Uncached
        }
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

    fn export_into(
        &self,
        out: &mut Vec<FrameFdPlane>,
    ) -> Result<Option<ExportedKind>, FrameExportError> {
        for &(offset, len) in &self.planes {
            let fd = self
                .buffer
                .fd
                .as_fd()
                .try_clone_to_owned()
                .map_err(FrameExportError::Fd)?;
            out.push(FrameFdPlane { fd, offset, len });
        }
        Ok(Some(ExportedKind::DmabufPlanes))
    }
}

impl BeBacking {
    /// Plane `index` in the buffer's mapping (made when the buffer was first leased).
    fn bytes(&self, index: usize) -> Option<&[u8]> {
        let &(offset, len) = self.planes.get(index)?;
        self.buffer
            .map
            .as_slice()
            .get(offset..offset.checked_add(len)?)
    }

    /// Starts (`true`) or ends CPU reads of the buffer.
    fn sync(&self, start: bool) {
        let _ = dma_heap::sync(self.buffer.fd.as_fd(), Access::Read, start);
    }
}

impl Drop for BeBacking {
    fn drop(&mut self) {
        self.window.close(|| self.sync(false));
        let _ = self.returns.send((self.output, self.index));
    }
}

/// Output buffers by (output, index), mapped and exported on first use.
#[derive(Default)]
pub(super) struct Buffers {
    cache: HashMap<(usize, u32), BeBuffer>,
}

impl Buffers {
    fn get(&mut self, p: &PispPipeline, i: usize, index: u32) -> Result<BeBuffer, String> {
        if let Some(b) = self.cache.get(&(i, index)) {
            return Ok(b.clone());
        }
        let fd = p
            .output_buffer_dmabuf(i, index)
            .and_then(|fd| fd.try_clone_to_owned().ok())
            .ok_or("no dma-buf")?;
        let size = p.output_buffer(i, index).map_or(0, <[u8]>::len);
        let dup = fd.try_clone().map_err(|e| e.to_string())?;
        let map = DmaBuf::from_fd(dup, size)
            .map()
            .map_err(|e| e.to_string())?;
        let b = BeBuffer {
            map: Arc::new(map),
            fd: Arc::new(fd),
            cached: p.output_cached(i),
        };
        self.cache.insert((i, index), b.clone());
        Ok(b)
    }
}

/// What an output buffer holds: the back end writes a crop or a scaled image into the top
/// left of the buffer, its rows in the buffer's stride and the chroma plane where the
/// buffer's height puts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Placed {
    /// The buffer's output: its format and size.
    pub(super) spec: OutputSpec,
    /// The buffer's row stride.
    pub(super) stride: usize,
    /// The image's size (`None`: the buffer's, or the crop's when there is one).
    pub(super) size: Option<(u32, u32)>,
    /// The part of the frame it shows (`FrameMeta::crop`; `None`: all of it).
    pub(super) crop: Option<FrameRect>,
}

/// Leases of one frame's output buffers.
pub(super) struct Leaser<'a> {
    pub(super) p: &'a PispPipeline,
    pub(super) f: &'a PispFrame,
    pub(super) buffers: &'a mut Buffers,
    pub(super) returns: &'a Returns,
    pub(super) live: &'a CaptureMetrics,
    /// How the frame's timestamp converts to the configured clock, sampled once for the frame
    /// (its companions convert the same way).
    pub(super) conversion: Option<ClockConversion>,
}

impl Leaser<'_> {
    /// A lease of output `i`'s buffer `index`, holding `placed`; on an error the buffer is
    /// still the caller's.
    pub(super) fn lease(
        &mut self,
        placed: Placed,
        (i, index): (usize, u32),
    ) -> Result<FrameLease, String> {
        let buffer = self.buffers.get(self.p, i, index)?;
        Ok(lease(
            placed,
            buffer,
            (i, index),
            self.f,
            self.returns,
            self.live,
            self.conversion,
        ))
    }
}

fn lease(
    placed: Placed,
    buffer: BeBuffer,
    (output, index): (usize, u32),
    f: &PispFrame,
    returns: &Returns,
    live: &CaptureMetrics,
    conversion: Option<ClockConversion>,
) -> FrameLease {
    let spec = placed.spec;
    let (width, height) = placed.size.unwrap_or_else(|| {
        placed
            .crop
            .map_or((spec.width, spec.height), |c| (c.width, c.height))
    });
    let in_buffer = layouts(spec.code, spec.height as usize, placed.stride);
    let rows = |len: usize| len / spec.height as usize * height as usize;
    let planes = in_buffer.iter().map(|l| (l.offset, rows(l.len))).collect();
    let plane_layouts = in_buffer
        .iter()
        .map(|l| PlaneLayout {
            offset: 0,
            len: rows(l.len),
            stride: l.stride,
        })
        .collect();
    let res = Resolution::new(width, height).expect("non-zero output size");
    let format = MediaFormat::new(spec.code, res, ColorSpace::Srgb);
    let mut meta = stamp_clock(
        FrameMeta::new(format, f.timestamp.as_nanos() as u64)
            .with_backend(BackendFrameMeta::Native(native_meta(f.sequence, &f.sensor)))
            .with_capture_instant(std::time::Instant::now()),
        conversion,
    );
    meta.crop = placed.crop;
    let dequeued = CaptureInstant::from(f.dequeued);
    meta.hops.set(Hop::Dequeued, dequeued.as_nanos());
    // The back end job (and its extra passes) done: the outputs were ready `total` after the
    // front end's buffers were dequeued.
    let done = CaptureInstant::from(f.dequeued + f.times.total);
    meta.hops.set(Hop::IspDone, done.as_nanos());
    FrameLease::from_external(
        meta,
        plane_layouts,
        Arc::new(live.track(BeBacking {
            buffer,
            planes,
            output,
            index,
            window: CpuReadWindow::new(),
            returns: returns.clone(),
        })),
    )
}

/// `rect` as the back end's crop config.
pub(super) fn be_crop(rect: FrameRect) -> BeCropConfig {
    let side = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
    BeCropConfig {
        offset_x: side(rect.x),
        offset_y: side(rect.y),
        width: side(rect.width),
        height: side(rect.height),
    }
}

#[cfg(test)]
#[path = "pisp_lease_alloc_tests.rs"]
mod alloc_tests;

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use styx_pipeline::SensorValues;
    use styx_pipeline::device::PispTimes;
    use styx_pisp::device::BeJob;

    use super::*;

    /// A back end buffer stand-in: a memfd holding an NV12 frame (Y = 1, CbCr = 2).
    pub(super) fn buffer(w: usize, h: usize) -> BeBuffer {
        let len = w * h * 3 / 2;
        let buf = DmaBuf::memfd("nv12", len).unwrap();
        let mut map = buf.map().unwrap();
        map.as_mut_slice()[..w * h].fill(1);
        map.as_mut_slice()[w * h..].fill(2);
        BeBuffer {
            map: Arc::new(map),
            fd: Arc::new(buf.into_fd()),
            cached: true,
        }
    }

    pub(super) fn frame() -> PispFrame {
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
            raw: None,
            digital_gain: 1.0,
            flicker: 1.0,
            times: PispTimes::default(),
            passes: Vec::new(),
        }
    }

    #[test]
    fn leases_carry_the_configured_clock() {
        let (w, h) = (64, 32);
        let spec = OutputSpec {
            code: FourCc::NV12,
            width: w as u32,
            height: h as u32,
        };
        let (tx, _rx) = returns();
        let live = CaptureMetrics::default();
        let placed = || Placed {
            spec,
            stride: w,
            size: None,
            crop: None,
        };
        let f = frame();
        let lease_in = |clock| {
            let conversion = crate::capture_api::native_backend::native_conversion(clock);
            lease(placed(), buffer(w, h), (0, 3), &f, &tx, &live, conversion)
        };
        let native = lease_in(ClockSource::Native);
        assert_eq!(native.meta().clock, Some(TimestampClock::Monotonic));
        assert_eq!(native.meta().timestamp, f.timestamp.as_nanos() as u64);
        let boot = lease_in(ClockSource::Boottime);
        assert_eq!(boot.meta().clock, Some(TimestampClock::Boottime));
        let back = boot.meta().timestamp_in(TimestampClock::Monotonic).unwrap();
        assert!(
            back.abs_diff(f.timestamp.as_nanos() as u64) < 10_000_000,
            "{back}"
        );
    }

    #[test]
    fn nv12_leases_export_their_planes_and_return_the_buffer() {
        let (w, h) = (64, 32);
        let spec = OutputSpec {
            code: FourCc::NV12,
            width: w as u32,
            height: h as u32,
        };
        let (tx, rx) = returns();
        let live = CaptureMetrics::default();
        let placed = Placed {
            spec,
            stride: w,
            size: None,
            crop: None,
        };
        let f = lease(placed, buffer(w, h), (0, 3), &frame(), &tx, &live, None);
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
        assert!(
            matches!(rx.recv(), RecvOutcome::Empty),
            "returned while held"
        );
        drop(planes);
        drop(f);
        assert!(matches!(rx.recv(), RecvOutcome::Data((0, 3))));
    }

    /// A crop sits in the top left of the full-size buffer: the planes keep the buffer's stride
    /// and the chroma plane its offset.
    #[test]
    fn cropped_leases_cover_the_crop_in_the_full_size_buffer() {
        let (w, h) = (64, 32);
        let spec = OutputSpec {
            code: FourCc::NV12,
            width: w as u32,
            height: h as u32,
        };
        let (tx, _rx) = returns();
        let live = CaptureMetrics::default();
        let crop = FrameRect::new(8, 4, 32, 16);
        let placed = Placed {
            spec,
            stride: w,
            size: None,
            crop: Some(crop),
        };
        let f = lease(placed, buffer(w, h), (0, 3), &frame(), &tx, &live, None);
        let res = f.meta().format.resolution;
        assert_eq!((res.width.get(), res.height.get()), (32, 16));
        assert_eq!(f.meta().crop, Some(crop));
        assert_eq!(f.plane_strides().as_slice(), &[w, w]);
        let planes = f.planes();
        assert_eq!(planes[0].data().len(), w * 16);
        assert!(planes[1].data().iter().all(|&v| v == 2));
        let FrameBackingExport::DmabufPlanes { planes: fds } = f.export_backing().unwrap() else {
            panic!("not a dma-buf export");
        };
        assert_eq!(
            fds.iter().map(|p| (p.offset, p.len)).collect::<Vec<_>>(),
            [(0, w * 16), (w * h, w * 8)]
        );
    }
}
