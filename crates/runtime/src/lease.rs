//! Frames handed on as `styx_core` [`FrameLease`]s: the receiver's buffer as the frame's
//! backing, without copying, given back to the receiver when the last view of the frame is
//! dropped.
//!
//! [`Frame`] stays the stream's item (typed by the receiver, no allocation: what the frame
//! loop and the sensor service need); [`Frame::into_lease`] is where it becomes the one frame
//! type consumers see, on Linux (`styx` hands the native capture's frames on this way) and on
//! a microcontroller alike. It costs the one allocation of the shared backing.

use smallvec::SmallVec;
use styx_core::buffer::{
    CpuAccess, ExternalBacking, FrameLease, FrameMeta, FrameResidency, PlaneLayout,
};
use styx_hal::{FrameBuffer, Receiver};

use crate::stream::Frame;

/// What a receiver's buffer is as a frame backing. Every method has a default for plain,
/// cached memory the receiver owns.
pub trait LeaseBuffer: FrameBuffer {
    /// How the CPU reads the buffer ([`CpuAccess::Cached`]: cached RAM, synced by
    /// [`FrameBuffer::begin_cpu`] on the first read).
    fn cpu_access(&self) -> CpuAccess {
        CpuAccess::Cached
    }

    /// Where the memory lives ([`FrameResidency::HostExternal`]: not the frame's own).
    fn residency(&self) -> FrameResidency {
        FrameResidency::HostExternal
    }

    /// A name for the backing ([`ExternalBacking::backing_kind`]).
    fn backing_kind(&self) -> &'static str {
        "receiver"
    }

    /// Whether [`Self::export_backing`] gives an fd.
    fn can_export(&self) -> bool {
        false
    }

    /// The buffer's dma-buf, borrowed, when it is one (for handing it to a device by
    /// descriptor without duplicating it: [`ExternalBacking::dmabuf_plane`]).
    #[cfg(all(feature = "std", unix))]
    fn dmabuf(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        None
    }

    /// The buffer for another process: its first `len` bytes as an fd (a dma-buf), `None`
    /// when it cannot be shared without a copy.
    #[cfg(all(feature = "std", unix))]
    fn export_backing(
        &self,
        _len: usize,
    ) -> Result<Option<styx_core::buffer::FrameBackingExport>, styx_core::buffer::FrameExportError>
    {
        Ok(None)
    }
}

/// A [`Frame`] as a [`FrameLease`]'s backing: every plane is read from the frame's bytes at
/// its layout's offset; dropping the last view gives the buffer back to the receiver.
pub struct FrameBacking<R: Receiver + ?Sized> {
    frame: Frame<R>,
    len: usize,
}

impl<R: Receiver + ?Sized> FrameBacking<R> {
    /// The frame.
    pub fn frame(&self) -> &Frame<R> {
        &self.frame
    }
}

impl<R> Frame<R>
where
    R: Receiver + Send + Sync + ?Sized + 'static,
    R::Buffer: LeaseBuffer + Send + Sync,
{
    /// This frame as a backing whose payload is its first `len` bytes (the planes' span).
    pub fn into_backing(self, len: usize) -> FrameBacking<R> {
        FrameBacking { frame: self, len }
    }

    /// This frame as a read-only [`FrameLease`] described by `meta` and `layouts` (offsets
    /// into the frame's bytes), without copying.
    pub fn into_lease(self, meta: FrameMeta, layouts: SmallVec<[PlaneLayout; 3]>) -> FrameLease {
        let len = layouts
            .iter()
            .map(|l| l.offset.saturating_add(l.len))
            .max()
            .unwrap_or(0);
        let mut meta = meta;
        if meta.residency.is_none() {
            meta.residency = Some(self.buffer().residency());
        }
        FrameLease::from_external(
            meta,
            layouts,
            styx_core::buffer::shared_backing(self.into_backing(len)),
        )
    }
}

impl<R> ExternalBacking for FrameBacking<R>
where
    R: Receiver + Send + Sync + ?Sized + 'static,
    R::Buffer: LeaseBuffer + Send + Sync,
{
    fn plane_data(&self, _index: usize) -> Option<&[u8]> {
        let buffer = self.frame.buffer();
        if !buffer.cpu_access().readable() {
            return None;
        }
        let data = self.frame.data();
        Some(&data[..self.len.min(data.len())])
    }

    fn begin_cpu_read(&self, _index: usize) -> Option<&[u8]> {
        if !self.frame.buffer().cpu_access().readable() {
            return None;
        }
        let data = self.frame.begin_read();
        Some(&data[..self.len.min(data.len())])
    }

    fn end_cpu_read(&self, _index: usize) {
        self.frame.end_read();
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.frame.buffer().len())
    }

    fn backing_kind(&self) -> &'static str {
        self.frame.buffer().backing_kind()
    }

    fn can_export(&self) -> bool {
        self.frame.buffer().can_export()
    }

    fn residency(&self) -> FrameResidency {
        self.frame.buffer().residency()
    }

    fn cpu_access(&self) -> CpuAccess {
        self.frame.buffer().cpu_access()
    }

    /// Every plane lies in the buffer, from its start (the layouts carry the offsets).
    #[cfg(all(feature = "std", unix))]
    fn dmabuf_plane(&self, _index: usize) -> Option<styx_core::buffer::DmabufPlane<'_>> {
        Some(styx_core::buffer::DmabufPlane {
            fd: self.frame.buffer().dmabuf()?,
            offset: 0,
        })
    }

    #[cfg(all(feature = "std", unix))]
    fn export_backing(
        &self,
    ) -> Result<Option<styx_core::buffer::FrameBackingExport>, styx_core::buffer::FrameExportError>
    {
        LeaseBuffer::export_backing(self.frame.buffer(), self.len)
    }
}

#[cfg(feature = "mock")]
impl LeaseBuffer for styx_hal::mock::MockBuffer {}
#[cfg(feature = "mock")]
impl LeaseBuffer for alloc::sync::Arc<styx_hal::mock::MockBuffer> {}
