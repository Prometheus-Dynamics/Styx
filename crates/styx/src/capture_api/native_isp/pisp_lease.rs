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
        let (sync, prefetch) = debug_override(cfg.early_cpu_sync, cfg.cpu_prefetch);
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

/// Debug hook (feature `pisp-debug-hooks`): `STYX_NATIVE_EARLY_SYNC=0|1` and
/// `STYX_NATIVE_PREFETCH=off|touch|l3` override the configured sync and prefetch (for
/// measurements).
#[cfg(feature = "pisp-debug-hooks")]
fn debug_override(
    sync: bool,
    prefetch: crate::capture_api::NativeCpuPrefetch,
) -> (bool, crate::capture_api::NativeCpuPrefetch) {
    use crate::capture_api::NativeCpuPrefetch as P;
    let sync = match std::env::var("STYX_NATIVE_EARLY_SYNC").as_deref() {
        Ok("0") => false,
        Ok("1") => true,
        _ => sync,
    };
    let prefetch = match std::env::var("STYX_NATIVE_PREFETCH").as_deref() {
        Ok("off") => P::Off,
        Ok("touch") => P::Touch,
        Ok("l3") => P::L3,
        _ => prefetch,
    };
    (sync, prefetch)
}

/// Without the feature, the configuration stands.
#[cfg(not(feature = "pisp-debug-hooks"))]
fn debug_override(
    sync: bool,
    prefetch: crate::capture_api::NativeCpuPrefetch,
) -> (bool, crate::capture_api::NativeCpuPrefetch) {
    (sync, prefetch)
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
            lease(
                placed(),
                buffer(w, h),
                (0, 3),
                &f,
                &tx,
                &live,
                conversion,
                CpuPrep::LAZY,
                None,
            )
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
        let f = lease(
            placed,
            buffer(w, h),
            (0, 3),
            &frame(),
            &tx,
            &live,
            None,
            CpuPrep::LAZY,
            None,
        );
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

    /// With early access (the default), the lease is handed over with CPU access already
    /// started: the consumer's reads find the window open; it closes when the lease drops.
    /// A prefetch reads the same pixels the lease shows.
    #[test]
    fn early_access_opens_the_window_on_the_capture_thread() {
        use crate::capture_api::NativeCpuPrefetch as P;
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
        for prefetch in [P::Off, P::Touch, P::L3] {
            let prep = CpuPrep {
                sync: true,
                prefetch,
            };
            let f = lease(
                placed,
                buffer(w, h),
                (0, 3),
                &frame(),
                &tx,
                &live,
                None,
                prep,
                None,
            );
            assert!(f.planes()[0].data().iter().all(|&v| v == 1), "{prefetch:?}");
            drop(f);
            assert!(matches!(rx.recv(), RecvOutcome::Data((0, 3))));
        }
        // Lazy: nothing until a read.
        let lazy = BeBacking {
            buffer: buffer(w, h),
            planes: SmallVec::from_slice(&[(0, w * h)]),
            output: 0,
            index: 1,
            window: CpuReadWindow::new(),
            returns: tx.clone(),
            progress: None,
        };
        assert!(!lazy.window.is_open());
        lazy.window.hold(|| lazy.sync(true));
        assert!(lazy.window.is_open());
        // A prefetch implies the sync: reading first would cache stale lines.
        let cfg = crate::capture_api::NativeIspConfig {
            early_cpu_sync: false,
            cpu_prefetch: P::L3,
            ..Default::default()
        };
        // The environment overrides only with the debug hooks (and when unset).
        let overridden = cfg!(feature = "pisp-debug-hooks")
            && (std::env::var_os("STYX_NATIVE_EARLY_SYNC").is_some()
                || std::env::var_os("STYX_NATIVE_PREFETCH").is_some());
        if !overridden {
            assert_eq!(
                CpuPrep::new(&cfg),
                CpuPrep {
                    sync: true,
                    prefetch: P::L3
                }
            );
            assert!(CpuPrep::new(&Default::default()).sync);
        }
    }

    /// A sensor window (Styx's OV9782 1280x480: rows 160..639 of the 1280x800 field) places
    /// a frame, and any back end crop of it, in the field's coordinates.
    #[test]
    fn crops_are_reported_in_the_sensors_field() {
        let w = styx_pisp::device::SensorWindow {
            x: 0,
            y: 160,
            width: 1280,
            height: 480,
            field: (1280, 800),
        };
        assert_eq!(in_field(w, None), FrameRect::new(0, 160, 1280, 480));
        let roi = FrameRect::new(100, 20, 256, 128);
        assert_eq!(in_field(w, Some(roi)), FrameRect::new(100, 180, 256, 128));
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
        let f = lease(
            placed,
            buffer(w, h),
            (0, 3),
            &frame(),
            &tx,
            &live,
            None,
            CpuPrep::LAZY,
            None,
        );
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
