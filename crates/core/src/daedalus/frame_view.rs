//! `daedalus:frame` v1 for [`FrameLease`]: Daedalus nodes take `FrameView<'_>` of a Styx frame,
//! including nodes in separately built plugins that know nothing of Styx, without a copy.
//!
//! The view reads the lease itself through Daedalus's generated vtable: plane pointers, lengths,
//! strides and offsets are the lease's own, and the payload (or the borrowed lease) keeps the
//! frame and its buffer alive for as long as the view lives.

use ::daedalus::transport::{
    DRM_FORMAT_MOD_INVALID, FramePlane, FrameResidency as ViewResidency, FrameSource,
};

use crate::buffer::{FrameLease, FrameResidency};
use crate::format::drm::{self, DRM_FORMAT_INVALID};

/// Where a view says `frame`'s memory lives: frames in their own host memory (and compressed
/// packets) are `Cpu`, memory owned elsewhere (dma-bufs, driver, memfd and caller buffers) is
/// `External`, GPU textures are `Gpu`; as [`payload_residency`](super::payload_residency). A
/// frame the CPU cannot read is never `Cpu`, which promises mapped planes.
pub fn view_residency(frame: &FrameLease) -> ViewResidency {
    match frame.residency() {
        FrameResidency::GpuTexture => ViewResidency::Gpu,
        FrameResidency::HostOwned | FrameResidency::CompressedPacket
            if frame.cpu_access().readable() =>
        {
            ViewResidency::Cpu
        }
        _ => ViewResidency::External,
    }
}

/// The frame as `daedalus:frame` v1 (docs/foreign-frame-interface.md in Daedalus).
///
/// - `format` / `modifier`: the DRM code of the frame's FourCC ([`drm::to_drm`]);
///   `DRM_FORMAT_INVALID` (0) and `DRM_FORMAT_MOD_INVALID` for formats without one
///   (compressed packets).
/// - `timestamp_ns`: the frame's timestamp (in its `clock`); `sequence`: the driver's sequence
///   number, 0 when unknown.
/// - planes: the lease's layouts. `data` is the plane's bytes in place only when the CPU can
///   read them ([`FrameLease::cpu_access`]; an uncached mapping is readable too, but slow), else
///   `None`, never a pointer into unmapped memory. `dmabuf_fd` and `offset` are the dma-buf the
///   plane lies in and its offset there ([`FrameLease::dmabuf_plane`]), borrowed from the
///   frame's backing; otherwise `offset` is the layout's offset in its buffer and there is no
///   fd. A frame with neither (a GPU texture, an unmapped backing that does not report its
///   dma-buf) shows its geometry but no bytes, and nodes refuse it.
impl FrameSource for FrameLease {
    fn width(&self) -> u32 {
        self.meta().format.resolution.width.get()
    }

    fn height(&self) -> u32 {
        self.meta().format.resolution.height.get()
    }

    fn format(&self) -> u32 {
        drm::to_drm(self.meta().format.code).map_or(DRM_FORMAT_INVALID, |f| f.fourcc)
    }

    fn modifier(&self) -> u64 {
        drm::to_drm(self.meta().format.code).map_or(DRM_FORMAT_MOD_INVALID, |f| f.modifier)
    }

    fn timestamp_ns(&self) -> u64 {
        self.meta().timestamp
    }

    fn sequence(&self) -> u64 {
        self.meta().sequence().map_or(0, u64::from)
    }

    fn residency(&self) -> ViewResidency {
        view_residency(self)
    }

    fn plane_count(&self) -> u32 {
        self.layout_slice().len() as u32
    }

    fn plane(&self, index: u32) -> Option<FramePlane<'_>> {
        let index = index as usize;
        let layout = *self.layout_slice().get(index)?;
        let data = if self.cpu_access().readable() {
            self.plane_at(index)
                .map(|plane| plane.data())
                .filter(|data| !data.is_empty())
        } else {
            None
        };
        let dmabuf = dmabuf(self, index);
        let offset = dmabuf.map_or(layout.offset, |(_, offset)| offset);
        Some(FramePlane {
            data,
            len: layout.len,
            stride: u32::try_from(layout.stride).unwrap_or(u32::MAX),
            offset: u32::try_from(offset).unwrap_or(u32::MAX),
            dmabuf_fd: dmabuf.map(|(fd, _)| fd),
        })
    }
}

/// Plane `index`'s dma-buf descriptor (borrowed) and offset in it.
#[cfg(unix)]
fn dmabuf(frame: &FrameLease, index: usize) -> Option<(i32, usize)> {
    use std::os::fd::AsRawFd;
    frame
        .dmabuf_plane(index)
        .map(|plane| (plane.fd.as_raw_fd(), plane.offset))
}

#[cfg(not(unix))]
fn dmabuf(_: &FrameLease, _: usize) -> Option<(i32, usize)> {
    None
}
