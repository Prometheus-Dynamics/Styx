//! `daedalus:frame` v2 for [`FrameLease`]: Daedalus nodes take `FrameView<'_>` of a Styx frame,
//! including nodes in separately built plugins that know nothing of Styx, without a copy.
//!
//! The view reads the lease itself through Daedalus's generated vtable. Metadata (geometry,
//! format, the planes' dma-bufs, offsets, strides and lengths) never touches the pixels: a
//! consumer that only forwards descriptors (a GPU importer) costs no `mmap` and no cache
//! maintenance. CPU bytes come from bracketed reads ([`FrameLease::begin_cpu_read`]), which
//! map lazily and sync dma-bufs while open. The payload (or the borrowed lease) keeps the frame
//! and its buffer alive for as long as the view lives.

use ::daedalus::transport::{
    DRM_FORMAT_MOD_INVALID, FrameFormatKind, FramePlane, FrameResidency as ViewResidency,
    FrameSource, PlaneMapping,
};

use crate::buffer::{CpuAccess, FrameLease, FrameResidency};
use crate::format::FourCc;
use crate::format::drm::{self, DRM_FORMAT_INVALID, DrmRegistry};

/// Where a view says `frame`'s memory lives: frames in their own host memory (and compressed
/// packets) are `Cpu`, memory owned elsewhere (dma-bufs, driver, memfd and caller buffers) is
/// `External`, GPU textures are `Gpu`; as [`payload_residency`](super::payload_residency). A
/// frame the CPU cannot read is never `Cpu`, which promises readable planes.
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

/// How a view says the CPU reads `frame`'s planes, from [`FrameLease::cpu_access`]: an
/// uncached or write-combined mapping is `Uncached`, [`CpuAccess::None`] is `Unmapped` (and
/// `plane_data` then gives nothing).
pub fn plane_mapping(frame: &FrameLease) -> PlaneMapping {
    match frame.cpu_access() {
        CpuAccess::Cached => PlaneMapping::Cached,
        CpuAccess::Uncached => PlaneMapping::Uncached,
        CpuAccess::None => PlaneMapping::Unmapped,
    }
}

/// What a view's `format` encodes for `code`: `Compressed` (the V4L2 fourcc of the bitstream),
/// `Bayer` (raw sensor data: libcamera's Bayer and raw codes, CSI-2 packed or not), `Pixel` (a
/// DRM fourcc), or `Unknown` when DRM has no code for it (format 0).
pub fn format_kind(code: FourCc) -> FrameFormatKind {
    if code.is_compressed() {
        return FrameFormatKind::Compressed;
    }
    match drm::mapping(code) {
        Some(m) if m.registry == DrmRegistry::Libcamera || code.is_bayer_raw() => {
            FrameFormatKind::Bayer
        }
        Some(_) => FrameFormatKind::Pixel,
        None => FrameFormatKind::Unknown,
    }
}

/// The frame as `daedalus:frame` v2 (docs/foreign-frame-interface.md in Daedalus).
///
/// - `format` / `format_kind` / `modifier`: the DRM code of the frame's FourCC
///   ([`drm::to_drm`]), `Pixel` or `Bayer` ([`format_kind`]; libcamera's
///   `MIPI_FORMAT_MOD_CSI2_PACKED` for CSI-2 packed raw); compressed packets are `Compressed`
///   with their V4L2 fourcc (`MJPG`, `H264`, ...) and `DRM_FORMAT_MOD_INVALID`; codes DRM has
///   none for are `Unknown`, format 0.
/// - `timestamp_ns`: the frame's timestamp (in its `clock`); `sequence`: the driver's sequence
///   number, 0 when unknown.
/// - `plane(i)`, metadata only (never maps or syncs): the layout's length and stride, and the
///   plane's dma-buf and offset in it ([`FrameLease::dmabuf_plane`], borrowed from the backing;
///   otherwise the layout's offset in its buffer, no fd), all `u64`; `mapping` from the frame's
///   CPU access ([`plane_mapping`]).
/// - `plane_data(i)` / `end_cpu_access(i)`: [`FrameLease::begin_cpu_read`] /
///   [`FrameLease::end_cpu_read`], the lease's own bytes in place, mapped on first use and
///   synced while reads are open; nothing when the CPU cannot read them (a GPU texture, an
///   unmapped dma-buf), never a pointer into unmapped memory.
impl FrameSource for FrameLease {
    fn width(&self) -> u32 {
        self.meta().format.resolution.width.get()
    }

    fn height(&self) -> u32 {
        self.meta().format.resolution.height.get()
    }

    fn format(&self) -> u32 {
        let code = self.meta().format.code;
        if code.is_compressed() {
            return code.to_u32();
        }
        drm::to_drm(code).map_or(DRM_FORMAT_INVALID, |f| f.fourcc)
    }

    fn format_kind(&self) -> FrameFormatKind {
        format_kind(self.meta().format.code)
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

    fn plane(&self, index: u32) -> Option<FramePlane> {
        let layout = *self.layout_slice().get(index as usize)?;
        let dmabuf = dmabuf(self, index as usize);
        Some(FramePlane {
            dmabuf_fd: dmabuf.map(|(fd, _)| fd),
            offset: dmabuf.map_or(layout.offset as u64, |(_, offset)| offset),
            stride: layout.stride as u64,
            len: layout.len as u64,
            mapping: plane_mapping(self),
        })
    }

    fn plane_data(&self, index: u32) -> Option<&[u8]> {
        self.begin_cpu_read(index as usize)
    }

    fn end_cpu_access(&self, index: u32) {
        self.end_cpu_read(index as usize);
    }
}

/// Plane `index`'s dma-buf descriptor (borrowed) and offset in it.
#[cfg(unix)]
fn dmabuf(frame: &FrameLease, index: usize) -> Option<(i32, u64)> {
    use std::os::fd::AsRawFd;
    frame
        .dmabuf_plane(index)
        .map(|plane| (plane.fd.as_raw_fd(), plane.offset as u64))
}

#[cfg(not(unix))]
fn dmabuf(_: &FrameLease, _: usize) -> Option<(i32, u64)> {
    None
}
