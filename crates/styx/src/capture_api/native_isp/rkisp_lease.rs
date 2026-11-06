//! Frames leased straight from the Rockchip ISP's main path buffers: each buffer is exported
//! (`VIDIOC_EXPBUF`) and mapped once when the stream starts, a lease holds its buffer and
//! hands its index back to the worker when it drops, and the worker queues it to the driver
//! again. The worker keeps [`DRIVER_MIN`] buffers with the driver: with fewer free it copies
//! the frame into a heap buffer instead, so consumers holding frames never stall the ISP.
//!
//! Status: written against the vendor driver's uAPI; not yet run on a board.

use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use lemnos_v4l2::Mapping;
use smallvec::SmallVec;
use styx_capture::prelude::*;
use styx_core::buffer::{BackingPool, CpuReadWindow};
use styx_core::prelude::{
    ExportedKind, ExternalBacking, FrameBackingExport, FrameExportError, FrameFdPlane,
    FrameResidency,
};
use styx_core::queue::{BoundedRx, BoundedTx, RecvOutcome};
use styx_native::dma::{self, Access, DmaBuf};
use styx_pipeline::device::RkispPipeline;

use super::layouts;
use crate::metrics::{CaptureMetrics, MeteredBacking};

/// Main path buffers always left with the driver (one being written, one queued next).
pub(super) const DRIVER_MIN: u32 = 2;

/// A main path buffer, exported and mapped once.
#[derive(Clone)]
struct RkBuffer {
    map: Arc<Mapping>,
    fd: Arc<OwnedFd>,
}

/// The main path's buffers, by index, and how many consumers hold.
pub(super) struct RkBuffers {
    buffers: Vec<RkBuffer>,
    held: u32,
    /// Buffers coming back from consumers (a ring: no allocation per frame).
    returns: BoundedTx<u32>,
    returned: BoundedRx<u32>,
    /// The leases' backing records, reused so a frame allocates none.
    backings: BackingPool<MeteredBacking<RkBacking>>,
}

impl RkBuffers {
    /// Exports and maps every main path buffer of `p`; `None` when the driver cannot export
    /// them (the worker copies every frame then).
    pub(super) fn new(p: &RkispPipeline) -> Option<Self> {
        let fds = (0..p.output_buffers())
            .map(|i| p.output_dmabuf(i).ok())
            .collect::<Option<Vec<_>>>()?;
        Self::from_fds(fds)
    }

    /// The buffers of these dma-bufs (and their lengths), by index.
    fn from_fds(fds: Vec<(OwnedFd, usize)>) -> Option<Self> {
        let buffers = fds
            .into_iter()
            .map(|(fd, len)| {
                let map = DmaBuf::from_fd(fd.try_clone().ok()?, len).map().ok()?;
                Some(RkBuffer {
                    map: Arc::new(map),
                    fd: Arc::new(fd),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let (returns, returned) = styx_core::queue::bounded(buffers.len().max(1));
        let backings = BackingPool::new(buffers.len().max(1));
        Some(Self {
            buffers,
            held: 0,
            returns,
            returned,
            backings,
        })
    }

    /// Hands the buffers consumers gave back to `release` (the driver's queue).
    pub(super) fn take_returns<E>(
        &mut self,
        mut release: impl FnMut(u32) -> Result<(), E>,
    ) -> Result<(), E> {
        while let RecvOutcome::Data(index) = self.returned.recv() {
            self.held = self.held.saturating_sub(1);
            release(index)?;
        }
        Ok(())
    }

    /// Whether one more buffer can go to consumers with [`DRIVER_MIN`] left with the driver.
    pub(super) fn can_lease(&self) -> bool {
        self.held + DRIVER_MIN < self.buffers.len() as u32
    }

    /// A lease of buffer `index` (an NV12 image of `size` at `stride`) with `meta`; the buffer
    /// stays out of the driver until the lease drops.
    pub(super) fn lease(
        &mut self,
        meta: FrameMeta,
        index: u32,
        (size, stride): ((u32, u32), usize),
        live: &CaptureMetrics,
    ) -> Option<FrameLease> {
        let buffer = self.buffers.get(index as usize)?.clone();
        let in_buffer = layouts(FourCc::NV12, size.1 as usize, stride);
        if in_buffer.iter().map(|l| l.offset + l.len).max()? > buffer.map.len() {
            return None;
        }
        let planes = in_buffer.iter().map(|l| (l.offset, l.len)).collect();
        let plane_layouts = in_buffer
            .iter()
            .map(|l| PlaneLayout {
                offset: 0,
                len: l.len,
                stride: l.stride,
            })
            .collect();
        self.held += 1;
        let backing = RkBacking {
            buffer,
            planes,
            index,
            window: CpuReadWindow::new(),
            returns: self.returns.clone(),
        };
        Some(FrameLease::from_external(
            meta,
            plane_layouts,
            self.backings.wrap(live.track(backing)),
        ))
    }
}

struct RkBacking {
    buffer: RkBuffer,
    /// Each plane's `(offset, len)` in the buffer.
    planes: SmallVec<[(usize, usize); 3]>,
    index: u32,
    window: CpuReadWindow,
    returns: BoundedTx<u32>,
}

impl RkBacking {
    fn bytes(&self, index: usize) -> Option<&[u8]> {
        let &(offset, len) = self.planes.get(index)?;
        self.buffer
            .map
            .as_slice()
            .get(offset..offset.checked_add(len)?)
    }

    fn sync(&self, start: bool) {
        let _ = dma::sync(self.buffer.fd.as_fd(), Access::Read, start);
    }
}

impl ExternalBacking for RkBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        let bytes = self.bytes(index)?;
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
        "rkisp_dmabuf"
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

    /// Mapped, so readable; the ISP's `mmap` of its buffers may be write-combined, so not
    /// claimed cached (written, untested on board).
    fn cpu_access(&self) -> CpuAccess {
        CpuAccess::Uncached
    }

    fn dmabuf_plane(&self, index: usize) -> Option<styx_core::buffer::DmabufPlane<'_>> {
        let &(offset, _) = self.planes.get(index)?;
        Some(styx_core::buffer::DmabufPlane {
            fd: self.buffer.fd.as_fd(),
            offset,
        })
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        let mut planes = Vec::with_capacity(self.planes.len());
        self.export_into(&mut planes)?;
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

impl Drop for RkBacking {
    fn drop(&mut self) {
        self.window.close(|| self.sync(false));
        let _ = self.returns.send(self.index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_alloc::allocations;

    const W: usize = 64;
    const H: usize = 32;

    fn buffers(n: usize) -> RkBuffers {
        let fds = (0..n)
            .map(|_| {
                let buf = DmaBuf::memfd("nv12", W * H * 3 / 2).unwrap();
                buf.map().unwrap().as_mut_slice()[W * H..].fill(2);
                (buf.into_fd(), W * H * 3 / 2)
            })
            .collect();
        RkBuffers::from_fds(fds).unwrap()
    }

    fn meta() -> FrameMeta {
        FrameMeta::new(
            MediaFormat::srgb(FourCc::NV12, W as u32, H as u32).unwrap(),
            0,
        )
    }

    #[test]
    fn two_buffers_stay_with_the_driver_and_dropped_leases_go_back_without_allocating() {
        let live = CaptureMetrics::default();
        let mut b = buffers(4);
        let size = ((W as u32, H as u32), W);
        let first = b.lease(meta(), 0, size, &live).unwrap();
        assert!(b.can_lease());
        let second = b.lease(meta(), 1, size, &live).unwrap();
        assert!(
            !b.can_lease(),
            "two of four out: the rest stay with the driver"
        );
        // The chroma plane is the buffer's, after the luma rows.
        assert_eq!(first.planes()[1].data()[0], 2);
        assert!(b.lease(meta(), 9, size, &live).is_none(), "no such buffer");
        drop(first);
        let mut released = Vec::new();
        b.take_returns(|i| {
            released.push(i);
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(released, [0]);
        assert!(b.can_lease());
        drop(second);
        // After a warm-up a frame allocates nothing: its lease, the return and the release.
        for _ in 0..10 {
            drop(b.lease(meta(), 2, size, &live).unwrap());
            b.take_returns(|_| Ok::<_, ()>(())).unwrap();
        }
        let m = meta();
        let (l, leased) = allocations(|| b.lease(m, 3, size, &live).unwrap());
        let ((), back) = allocations(|| {
            drop(l);
            b.take_returns(|_| Ok::<_, ()>(())).unwrap();
        });
        assert_eq!((leased, back), (0, 0));
    }
}
