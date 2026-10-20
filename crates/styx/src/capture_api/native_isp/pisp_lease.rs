//! Frames leased straight from the PiSP back end's output buffers: each buffer is mapped and
//! exported once (cached per buffer index) and shared by the leases that hold it; a lease
//! returns its buffer to the back end when it drops, so the outputs of a job (and its extra
//! passes) can live for different times. The CPU-access sync (`DMA_BUF_IOCTL_SYNC`) of a
//! cached buffer starts on the capture thread as the frame is leased ([`CpuPrep`], the
//! default), or else when someone first reads the pixels.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use smallvec::SmallVec;
use styx_capture::prelude::*;
use styx_core::buffer::{CpuReadWindow, FrameProgressRef};

use super::pisp_sliced::SliceState;
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
use super::{aaa_sample, err, layouts, native_aaa, native_meta};
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

/// What the capture thread does to a cached buffer before handing its lease over
/// (`NativeIspConfig::early_cpu_sync` and `cpu_prefetch`, or their environment overrides).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CpuPrep {
    /// Start CPU access (the cache sync) now rather than on the consumer's first read.
    pub(super) sync: bool,
    /// Then prefetch the first plane.
    pub(super) prefetch: crate::capture_api::NativeCpuPrefetch,
}

impl CpuPrep {
    /// From the configuration.
    pub(super) fn new(cfg: &crate::capture_api::NativeIspConfig) -> Self {
        use crate::capture_api::NativeCpuPrefetch as P;
        let (sync, prefetch) = (cfg.early_cpu_sync, cfg.cpu_prefetch);
        // Reading before the sync would cache stale lines: a prefetch starts CPU access.
        Self {
            sync: sync || prefetch != P::Off,
            prefetch,
        }
    }

    /// Nothing early: the sync on the first read.
    #[cfg(test)]
    pub(super) const LAZY: Self = Self {
        sync: false,
        prefetch: crate::capture_api::NativeCpuPrefetch::Off,
    };
}

/// Brings `bytes` into the caches: a load of every 64-byte line (`Touch`), or a level-3
/// prefetch hint for each (`L3`, which does not wait for the data).
fn prefetch(bytes: &[u8], how: crate::capture_api::NativeCpuPrefetch) {
    use crate::capture_api::NativeCpuPrefetch as P;
    const LINE: usize = 64;
    match how {
        P::Off => {}
        #[cfg(target_arch = "aarch64")]
        P::L3 => {
            for line in bytes.chunks(LINE) {
                // SAFETY: a prefetch hint only; the address is inside `bytes`.
                unsafe {
                    core::arch::asm!("prfm pldl3keep, [{0}]", in(reg) line.as_ptr(), options(nostack, readonly, preserves_flags));
                }
            }
        }
        _ => {
            let mut acc = 0u8;
            for line in bytes.chunks(LINE) {
                acc ^= line[0];
            }
            std::hint::black_box(acc);
        }
    }
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
    /// A sliced frame's state (shared by its leases) and this output's index in its progress.
    progress: Option<(Arc<SliceState>, usize)>,
}

impl ExternalBacking for BeBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        let bytes = self.bytes(index)?;
        // The pixels are about to be read on the CPU: sync once (a no-op for the driver's
        // uncached buffers, cache maintenance for cached ones).
        self.window.hold(|| self.start_read());
        Some(bytes)
    }

    fn begin_cpu_read(&self, index: usize) -> Option<&[u8]> {
        let bytes = self.bytes(index)?;
        self.window.begin(|| self.start_read());
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

    fn host_spans(&self, span: &mut dyn FnMut(usize, usize)) -> bool {
        let bytes = self.buffer.map.as_slice();
        span(bytes.as_ptr() as usize, bytes.len());
        true
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

    fn progress(&self) -> Option<FrameProgressRef<'_>> {
        self.progress.as_ref().map(|(s, i)| s.progress.output(*i))
    }

    fn progress_handle(&self) -> Option<styx_core::buffer::FrameProgressHandle> {
        let (s, i) = self.progress.as_ref()?;
        Some(styx_core::buffer::FrameProgressHandle::new(
            s.progress.clone(),
            *i,
        ))
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

    /// Starts CPU reads of the buffer (marking a sliced frame read early).
    fn start_read(&self) {
        if let Some((s, _)) = &self.progress {
            s.read.store(true, std::sync::atomic::Ordering::Release);
        }
        self.sync(true);
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
    /// Output `i`'s buffer `index`'s dma-buf and whether it is cached, when mapped already.
    pub(super) fn fd(&self, i: usize, index: u32) -> Option<(std::os::fd::BorrowedFd<'_>, bool)> {
        self.cache
            .get(&(i, index))
            .map(|b| (b.fd.as_fd(), b.cached))
    }

    fn get(&mut self, p: &PispPipeline, i: usize, index: u32) -> Result<BeBuffer, String> {
        if let Some(b) = self.cache.get(&(i, index)) {
            return Ok(b.clone());
        }
        let (fd, size) = if i == RAW_OUTPUT {
            let layout = p.raw_layout().ok_or("no raw layout")?;
            (
                p.raw_dmabuf(index),
                layout.stride as usize * layout.height as usize,
            )
        } else {
            (
                p.output_buffer_dmabuf(i, index),
                p.output_buffer(i, index).map_or(0, <[u8]>::len),
            )
        };
        let fd = fd
            .and_then(|fd| fd.try_clone_to_owned().ok())
            .ok_or("no dma-buf")?;
        let dup = fd.try_clone().map_err(|e| e.to_string())?;
        let map = DmaBuf::from_fd(dup, size)
            .map()
            .map_err(|e| e.to_string())?;
        let b = BeBuffer {
            map: Arc::new(map),
            fd: Arc::new(fd),
            // The front end's raw buffers are the driver's, uncached.
            cached: i != RAW_OUTPUT && p.output_cached(i),
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
    /// What the capture thread does to a cached buffer before handing it over.
    pub(super) prep: CpuPrep,
    /// The window of the sensor's field the mode reads, if it reads one: frame crops are
    /// reported in the field's coordinates.
    pub(super) window: Option<styx_pisp::device::SensorWindow>,
    /// A sliced frame's progress, for leases handed out before the frame completes.
    pub(super) progress: Option<Arc<SliceState>>,
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
        let aaa = native_aaa(
            &aaa_sample(&self.p.step().params, self.p.controls(), self.f.sequence),
            self.p.step().isp.wb,
        );
        Ok(lease(
            placed,
            buffer,
            (i, index),
            self.f,
            self.returns,
            self.live,
            self.conversion,
            self.prep,
            self.progress.clone(),
            aaa,
        ))
        .map(|mut l| {
            if let Some(w) = self.window {
                let crop = in_field(w, l.meta().crop);
                l.meta_mut().crop = Some(crop);
            }
            l
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn lease(
    placed: Placed,
    buffer: BeBuffer,
    (output, index): (usize, u32),
    f: &PispFrame,
    returns: &Returns,
    live: &CaptureMetrics,
    conversion: Option<ClockConversion>,
    prep: CpuPrep,
    progress: Option<Arc<SliceState>>,
    aaa: styx_core::buffer::NativeAaa,
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
            .with_backend(BackendFrameMeta::Native(NativeFrameMeta {
                frame_end_ns: CaptureInstant::from(f.dequeued).as_nanos(),
                aaa: Some(aaa),
                ..native_meta(f.sequence, &f.sensor)
            }))
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
    let backing = BeBacking {
        buffer,
        planes,
        output,
        index,
        window: CpuReadWindow::new(),
        returns: returns.clone(),
        progress: progress.map(|p| (p, output)),
    };
    if prep.sync && backing.buffer.cached {
        // Held from now until the lease drops: the consumer's reads find access started.
        backing.window.hold(|| backing.start_read());
        if let Some(first) = backing.bytes(0) {
            prefetch(first, prep.prefetch);
        }
    }
    FrameLease::from_external(meta, plane_layouts, Arc::new(live.track(backing)))
}

/// The pseudo-output the front end's raw buffers come back to the worker as (the back end's
/// outputs are 0 and 1).
pub(super) const RAW_OUTPUT: usize = 2;

/// A lease of the frame's raw front end buffer ([`PispFrame::raw_image`]): 16-bit Bayer
/// samples (the sensor's value in the top bits), as the back end read them. `None` when the
/// frame has no held raw buffer or it cannot be mapped (the caller gives it back then).
pub(super) fn raw_lease(
    p: &PispPipeline,
    buffers: &mut Buffers,
    f: &PispFrame,
    returns: &Returns,
    live: &CaptureMetrics,
    conversion: Option<ClockConversion>,
    progress: Option<Arc<SliceState>>,
) -> Option<FrameLease> {
    let index = f.raw_image?;
    let layout = p.raw_layout()?;
    let buffer = buffers.get(p, RAW_OUTPUT, index).ok()?;
    let code = FourCc::new(*match layout.cfa {
        styx_softisp::CfaPattern::Bggr => b"BYR2",
        styx_softisp::CfaPattern::Gbrg => b"GB16",
        styx_softisp::CfaPattern::Grbg => b"GR16",
        styx_softisp::CfaPattern::Rggb => b"RG16",
    });
    let len = layout.stride as usize * layout.height as usize;
    let format = MediaFormat::new(
        code,
        Resolution::new(layout.width, layout.height)?,
        ColorSpace::Unknown,
    );
    let mut meta = stamp_clock(
        FrameMeta::new(format, f.timestamp.as_nanos() as u64)
            .with_backend(BackendFrameMeta::Native(NativeFrameMeta {
                frame_end_ns: CaptureInstant::from(f.dequeued).as_nanos(),
                ..native_meta(f.sequence, &f.sensor)
            }))
            .with_capture_instant(std::time::Instant::now()),
        conversion,
    );
    meta.hops
        .set(Hop::Dequeued, CaptureInstant::from(f.dequeued).as_nanos());
    let backing = BeBacking {
        buffer,
        planes: smallvec::smallvec![(0, len)],
        output: RAW_OUTPUT,
        index,
        window: CpuReadWindow::new(),
        returns: returns.clone(),
        progress: progress.map(|p| (p, 0)),
    };
    let layouts = smallvec::smallvec![PlaneLayout {
        offset: 0,
        len,
        stride: layout.stride as usize,
    }];
    Some(FrameLease::from_external(
        meta,
        layouts,
        Arc::new(live.track(backing)),
    ))
}

/// `crop` (of the frame the sensor's window `w` reads; `None`: all of it) in the coordinates of
/// the sensor's full field.
pub(super) fn in_field(w: styx_pisp::device::SensorWindow, crop: Option<FrameRect>) -> FrameRect {
    match crop {
        Some(c) => FrameRect::new(w.x + c.x, w.y + c.y, c.width, c.height),
        None => FrameRect::new(w.x, w.y, w.width, w.height),
    }
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
#[path = "pisp_lease_tests.rs"]
mod tests;
