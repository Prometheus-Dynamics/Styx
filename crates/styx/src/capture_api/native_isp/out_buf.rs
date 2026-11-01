//! Output buffers the CPU writes frames into (the software ISP's outputs, the Rockchip path's
//! fallback copies): from a cached dma-heap (`linux,cma`, else `system`), so consumers get
//! dma-bufs a GPU or another process imports without a copy, else plain heap memory (no heap,
//! or `NativeIspConfig::driver_buffers`). Recycled: a lease hands its buffer back when it
//! drops, and a capture allocates only until its buffers go round.

use std::os::fd::AsFd;
use std::sync::Arc;

use smallvec::SmallVec;
use styx_capture::prelude::*;
use styx_core::prelude::{
    ExportedKind, ExternalBacking, FrameBackingExport, FrameExportError, FrameFdPlane,
    FrameResidency,
};
use styx_core::queue::{BoundedRx, BoundedTx, RecvOutcome};
use styx_kernel::Mapping;
use styx_kernel::dma_heap::{self, Access, DmaBuf, DmaHeap};

use super::layouts;
use crate::metrics::CaptureMetrics;

/// Where leases hand their buffers back (a ring: no allocation per frame; a buffer coming
/// back to a full ring is freed).
pub(super) type Returns = BoundedTx<OutBuf>;

/// The worker's end of [`Returns`].
pub(super) type ReturnsRx = BoundedRx<OutBuf>;

/// Room for buffers coming back to one output (more than consumers hold at once).
const RETURNS: usize = 64;

/// A ring for buffers coming back.
pub(super) fn returns() -> (Returns, ReturnsRx) {
    styx_core::queue::bounded(RETURNS)
}

/// Heaps tried for the buffers, cached ones first.
const HEAPS: [&str; 2] = ["linux,cma", "system"];

/// The heap output buffers come from: `None` for plain heap memory (`driver`, or no heap).
pub(super) fn out_heap(driver: bool) -> Option<Arc<DmaHeap>> {
    if driver {
        return None;
    }
    HEAPS
        .iter()
        .find_map(|n| DmaHeap::open(n).ok())
        .map(Arc::new)
}

/// A buffer of [`Self::len`] bytes the CPU writes a frame into.
pub(super) struct OutBuf {
    mem: Mem,
    len: usize,
}

enum Mem {
    Heap(Vec<u8>),
    Dma { buf: DmaBuf, map: Mapping },
}

impl OutBuf {
    /// The frame's bytes.
    pub(super) fn as_slice(&self) -> &[u8] {
        match &self.mem {
            Mem::Heap(v) => &v[..self.len],
            Mem::Dma { map, .. } => &map.as_slice()[..self.len],
        }
    }

    /// The frame's bytes, to write.
    pub(super) fn as_mut_slice(&mut self) -> &mut [u8] {
        match &mut self.mem {
            Mem::Heap(v) => &mut v[..self.len],
            Mem::Dma { map, .. } => &mut map.as_mut_slice()[..self.len],
        }
    }

    /// Brackets the CPU's writes (`DMA_BUF_IOCTL_SYNC`; nothing for heap memory): `true`
    /// before, `false` after, so a device reading the dma-buf sees them.
    pub(super) fn sync_write(&self, start: bool) {
        if let Mem::Dma { buf, .. } = &self.mem {
            let _ = dma_heap::sync(buf.as_fd(), Access::Write, start);
        }
    }

    fn capacity(&self) -> usize {
        match &self.mem {
            Mem::Heap(v) => v.capacity(),
            Mem::Dma { map, .. } => map.len(),
        }
    }
}

/// One output's spare buffers.
#[derive(Default)]
pub(super) struct Spare(Vec<OutBuf>);

impl Spare {
    /// A buffer of `len` bytes: one that came back through `returns`, else a new one from
    /// `heap` (heap memory without one, or if it fails). At most `keep` buffers stay spare;
    /// more are freed, so frames once held at the same time do not keep their memory for the
    /// rest of the capture.
    pub(super) fn take(
        &mut self,
        heap: Option<&DmaHeap>,
        returns: &ReturnsRx,
        keep: usize,
        len: usize,
    ) -> OutBuf {
        while let RecvOutcome::Data(b) = returns.recv() {
            self.0.push(b);
        }
        self.0.truncate(keep.max(1));
        match self.0.pop() {
            Some(mut b) if b.capacity() >= len => {
                if let Mem::Heap(v) = &mut b.mem {
                    v.resize(len, 0);
                }
                b.len = len;
                b
            }
            Some(OutBuf {
                mem: Mem::Heap(mut v),
                ..
            }) if heap.is_none() => {
                v.resize(len, 0);
                OutBuf {
                    mem: Mem::Heap(v),
                    len,
                }
            }
            _ => allocate(heap, len),
        }
    }
}

fn allocate(heap: Option<&DmaHeap>, len: usize) -> OutBuf {
    let dma = heap.and_then(|h| {
        let buf = h.allocate(len.next_multiple_of(4096).max(4096)).ok()?;
        let map = buf.map().ok()?;
        Some(Mem::Dma { buf, map })
    });
    OutBuf {
        mem: dma.unwrap_or_else(|| Mem::Heap(vec![0; len])),
        len,
    }
}

/// A lease of `buf`, a `code` image of `size` with `meta`, recycled through `returns`.
pub(super) fn lease(
    mut meta: FrameMeta,
    size: (u32, u32),
    buf: OutBuf,
    returns: &Returns,
    live: &CaptureMetrics,
) -> FrameLease {
    let code = meta.format.code;
    let res = Resolution::new(size.0, size.1).expect("non-zero image size");
    meta.format = MediaFormat::new(code, res, meta.format.color);
    let planes = layouts(code, size.1 as usize, row_bytes(code, size.0));
    let offsets = planes.iter().map(|l| l.offset).collect();
    FrameLease::from_external(
        meta,
        planes,
        Arc::new(live.track(OutBacking {
            buf: Some(buf),
            offsets,
            returns: returns.clone(),
        })),
    )
}

/// Bytes per row of a `code` image `w` pixels wide in these buffers.
pub(super) fn row_bytes(code: FourCc, w: u32) -> usize {
    if code == FourCc::NV12 {
        w as usize
    } else {
        w as usize * 3
    }
}

/// An output buffer held by a lease, handed back through `returns` when it drops.
struct OutBacking {
    buf: Option<OutBuf>,
    /// Each plane's offset in the buffer.
    offsets: SmallVec<[usize; 3]>,
    returns: Returns,
}

impl OutBacking {
    fn dma(&self) -> Option<&DmaBuf> {
        match &self.buf.as_ref()?.mem {
            Mem::Dma { buf, .. } => Some(buf),
            Mem::Heap(_) => None,
        }
    }
}

impl ExternalBacking for OutBacking {
    fn plane_data(&self, _index: usize) -> Option<&[u8]> {
        self.buf.as_ref().map(OutBuf::as_slice)
    }

    fn backing_bytes(&self) -> Option<usize> {
        self.buf.as_ref().map(|b| b.len)
    }

    fn backing_kind(&self) -> &'static str {
        if self.dma().is_some() {
            "softisp_dmabuf"
        } else {
            "softisp_heap"
        }
    }

    fn host_spans(&self, span: &mut dyn FnMut(usize, usize)) -> bool {
        let data = self.buf.as_ref().map(OutBuf::as_slice);
        data.map(|b| span(b.as_ptr() as usize, b.len())).is_some()
    }

    fn can_export(&self) -> bool {
        self.dma().is_some()
    }

    fn residency(&self) -> FrameResidency {
        if self.dma().is_some() {
            FrameResidency::Dmabuf
        } else {
            FrameResidency::HostExternal
        }
    }

    fn dmabuf_plane(&self, index: usize) -> Option<styx_core::buffer::DmabufPlane<'_>> {
        let offset = *self.offsets.get(index)?;
        Some(styx_core::buffer::DmabufPlane {
            fd: self.dma()?.as_fd(),
            offset,
        })
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        if self.dma().is_none() {
            return Ok(None);
        }
        let mut planes = Vec::with_capacity(self.offsets.len());
        self.export_into(&mut planes)?;
        Ok(Some(FrameBackingExport::DmabufPlanes { planes }))
    }

    fn export_into(
        &self,
        out: &mut Vec<FrameFdPlane>,
    ) -> Result<Option<ExportedKind>, FrameExportError> {
        let (Some(dma), Some(b)) = (self.dma(), self.buf.as_ref()) else {
            return Ok(None);
        };
        for (k, &offset) in self.offsets.iter().enumerate() {
            let end = self.offsets.get(k + 1).copied().unwrap_or(b.len);
            let fd = dma
                .as_fd()
                .try_clone_to_owned()
                .map_err(FrameExportError::Fd)?;
            out.push(FrameFdPlane {
                fd,
                offset,
                len: end - offset,
            });
        }
        Ok(Some(ExportedKind::DmabufPlanes))
    }
}

impl Drop for OutBacking {
    fn drop(&mut self) {
        if let Some(b) = self.buf.take() {
            let _ = self.returns.send(b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_come_back_and_grow_only_when_too_small() {
        let (tx, rx) = returns();
        let mut spare = Spare::default();
        let live = CaptureMetrics::default();
        let mut b = spare.take(None, &rx, 2, 64 * 48);
        b.as_mut_slice()[64 * 32] = 9;
        let meta = FrameMeta::new(MediaFormat::srgb(FourCc::NV12, 64, 32).unwrap(), 0);
        let l = lease(meta, (64, 32), b, &tx, &live);
        assert_eq!(l.planes()[1].data()[0], 9);
        assert_eq!(l.residency(), FrameResidency::HostExternal);
        drop(l);
        // Back through the channel: the same memory, at a smaller size, then a larger one.
        let b = spare.take(None, &rx, 2, 32 * 24);
        assert_eq!(b.as_slice().len(), 32 * 24);
        drop(b);
        let b = spare.take(None, &rx, 2, 128 * 96);
        assert_eq!(b.as_slice().len(), 128 * 96);
    }

    #[test]
    fn heap_buffers_are_dmabufs_consumers_import() {
        // Where no dma-heap is open to this user, the heap-memory path is what runs.
        let Some(heap) = out_heap(false) else { return };
        let (tx, rx) = returns();
        let mut spare = Spare::default();
        let mut b = spare.take(Some(&heap), &rx, 2, 64 * 48);
        b.sync_write(true);
        b.as_mut_slice()[64 * 32] = 7;
        b.sync_write(false);
        let meta = FrameMeta::new(MediaFormat::srgb(FourCc::NV12, 64, 32).unwrap(), 0);
        let l = lease(meta, (64, 32), b, &tx, &CaptureMetrics::default());
        assert_eq!(l.residency(), FrameResidency::Dmabuf);
        assert_eq!(l.planes()[1].data()[0], 7);
        let Ok(FrameBackingExport::DmabufPlanes { planes }) = l.export_backing() else {
            panic!("no dma-buf export");
        };
        assert_eq!(
            planes.iter().map(|p| (p.offset, p.len)).collect::<Vec<_>>(),
            [(0, 64 * 32), (64 * 32, 64 * 16)]
        );
    }

    /// The soft ISP's output path after a warm-up: a buffer back from the consumer, written,
    /// leased, delivered, taken, read and dropped; each frame allocates its lease record only.
    fn frames_allocate_only_their_lease_record(heap: Option<&DmaHeap>) {
        use crate::test_alloc::allocations;
        use styx_core::queue::RecvOutcome;
        const W: u32 = 320;
        const H: u32 = 200;
        let live = CaptureMetrics::default();
        let (ret_tx, ret_rx) = returns();
        let (queue_tx, queue_rx) = styx_core::queue::bounded(2);
        let mut spare = Spare::default();
        let format = MediaFormat::srgb(FourCc::NV12, W, H).unwrap();
        let (mut leased, mut path) = (0u64, 0u64);
        let mut xor = 0u8;
        for n in 0..300 {
            let (b, took) = allocations(|| {
                let mut b = spare.take(heap, &ret_rx, 2, (W * H * 3 / 2) as usize);
                b.sync_write(true);
                b.as_mut_slice()[n % 64] = n as u8;
                b.sync_write(false);
                b
            });
            let (l, lease_allocs) =
                allocations(|| lease(FrameMeta::new(format, n as u64), (W, H), b, &ret_tx, &live));
            let (sent, delivered) = allocations(|| queue_tx.send(l));
            assert!(matches!(sent, styx_core::queue::SendOutcome::Ok));
            let ((), consumed) = allocations(|| {
                let RecvOutcome::Data(frame) = queue_rx.recv() else {
                    panic!("no frame");
                };
                for plane in frame.planes() {
                    xor ^= plane.data().iter().step_by(4096).fold(0, |a, b| a ^ b);
                }
            });
            if n >= 50 {
                leased += lease_allocs;
                path += took + delivered + consumed;
            }
        }
        std::hint::black_box(xor);
        assert_eq!(path, 0, "the frame path allocates");
        assert_eq!(leased, 250, "a lease allocates more than its record");
    }

    #[test]
    fn heap_memory_frames_allocate_only_their_lease_record() {
        frames_allocate_only_their_lease_record(None);
    }

    #[test]
    fn dmabuf_frames_allocate_only_their_lease_record() {
        if let Some(heap) = out_heap(false) {
            frames_allocate_only_their_lease_record(Some(&heap));
        }
    }
}
