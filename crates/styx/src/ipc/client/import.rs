//! Importing a received frame over its descriptors: the cached mappings, and the release the
//! server gets once the frame and its companions are dropped.

use std::os::fd::OwnedFd;
use std::sync::Arc;

use smallvec::SmallVec;
use styx_core::prelude::*;

use super::super::mapcache::{CachedDmabuf, Fds, Inner, MapCache};
use super::super::wire::{self, ClientHops, WireBacking, WireFrame};
use super::super::{IpcError, socket};

/// `frame` over the descriptors `fds` yields, released on the server with `release` once it
/// and its companions are dropped. Its hops get the receive (from `release`) and import times.
pub(super) fn import(
    frame: &WireFrame,
    fds: &mut impl Iterator<Item = OwnedFd>,
    mut release: Release,
    maps: &Arc<MapCache>,
) -> Result<FrameLease, IpcError> {
    let imported = CaptureInstant::try_now().map(CaptureInstant::as_nanos);
    release.hops.imported = imported;
    let mut meta = frame.meta.clone();
    if let Some(ns) = release.hops.received {
        meta.hops.set(Hop::Received, ns);
    }
    if let Some(ns) = imported {
        meta.hops.set(Hop::Imported, ns);
    }
    let handle = if frame.companions.is_empty() {
        ReleaseHandle::Own(release)
    } else {
        ReleaseHandle::Shared(Arc::new(release))
    };
    let shared = match &handle {
        ReleaseHandle::Shared(r) => Some(r.clone()),
        ReleaseHandle::Own(_) => None,
    };
    let mut out = import_part(frame, meta, fds, handle, maps)?;
    for (kind, companion) in &frame.companions {
        let release = ReleaseHandle::Shared(shared.clone().expect("shared with companions"));
        let part = import_part(companion, companion.meta.clone(), fds, release, maps)?;
        out = out
            .with_companion(*kind, part)
            .map_err(|_| IpcError::Malformed("companion does not match its frame"))?;
    }
    Ok(out)
}

fn import_part(
    frame: &WireFrame,
    mut meta: FrameMeta,
    fds: &mut impl Iterator<Item = OwnedFd>,
    release: ReleaseHandle,
    maps: &Arc<MapCache>,
) -> Result<FrameLease, IpcError> {
    let count = frame.backing.fd_count();
    let own: Fds = fds.by_ref().take(count).collect();
    if own.len() != count {
        return Err(IpcError::Malformed("descriptors missing"));
    }
    let layouts: SmallVec<[PlaneLayout; 3]> = frame.layouts.iter().copied().collect();
    let inner = match &frame.backing {
        WireBacking::Dmabuf(spans) => {
            if spans.len() != layouts.len() {
                return Err(FrameExportError::PlaneCountMismatch {
                    expected: layouts.len(),
                    actual: spans.len(),
                }
                .into());
            }
            meta.residency = Some(FrameResidency::Dmabuf);
            // Planes on one buffer (the usual case) read through the cached mappings.
            match CachedDmabuf::new(maps, own, spans) {
                Ok(cached) => Inner::Cached(cached),
                Err(own) => {
                    let planes = own
                        .into_iter()
                        .zip(spans)
                        .map(|(fd, &(offset, len))| FrameFdPlane { fd, offset, len })
                        .collect();
                    let f = FrameLease::from_dmabuf(meta.clone(), layouts.clone(), planes)?;
                    Inner::Other(external(&f)?)
                }
            }
        }
        WireBacking::Memfd { len } => {
            let fd = own.into_iter().next().expect("one descriptor");
            // Read through the cached mappings too (a capture cycles through its memfds).
            match CachedDmabuf::memfd(maps, fd, *len, layouts.len()) {
                Ok(cached) => {
                    meta.residency = Some(FrameResidency::HostExternal);
                    Inner::Cached(cached)
                }
                Err(fd) => {
                    let f = FrameLease::from_memfd(meta.clone(), layouts.clone(), fd);
                    meta.residency = f.meta().residency;
                    Inner::Other(external(&f)?)
                }
            }
        }
    };
    Ok(FrameLease::from_external(
        meta,
        layouts,
        Arc::new(Released {
            inner,
            _release: release,
            cpu_access: frame.cpu_access,
        }),
    ))
}

fn external(frame: &FrameLease) -> Result<Arc<dyn ExternalBacking>, IpcError> {
    frame
        .external_backing_handle()
        .ok_or(IpcError::Malformed("frame without backing"))
}

/// Tells the server a frame was dropped, so it can let go of its buffers; with the frame's
/// receive and import times here.
pub(super) struct Release {
    pub(super) socket: Arc<OwnedFd>,
    pub(super) id: u64,
    pub(super) hops: ClientHops,
}

impl Drop for Release {
    fn drop(&mut self) {
        let _ = socket::send(&self.socket, &wire::release_bytes(self.id, self.hops), &[]);
    }
}

/// A frame's release: its own (no companions), or shared with its companions.
enum ReleaseHandle {
    Own(#[allow(dead_code)] Release),
    Shared(#[allow(dead_code)] Arc<Release>),
}

/// A received frame's memory; the frame is released on the server once it and its companions
/// are all dropped.
struct Released {
    inner: Inner,
    _release: ReleaseHandle,
    /// As the sender reported it for its memory.
    cpu_access: CpuAccess,
}

impl ExternalBacking for Released {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        self.inner.backing().plane_data(index)
    }

    fn begin_cpu_read(&self, index: usize) -> Option<&[u8]> {
        self.inner.backing().begin_cpu_read(index)
    }

    fn end_cpu_read(&self, index: usize) {
        self.inner.backing().end_cpu_read(index)
    }

    fn backing_bytes(&self) -> Option<usize> {
        self.inner.backing().backing_bytes()
    }

    fn backing_kind(&self) -> &'static str {
        "ipc"
    }

    fn can_export(&self) -> bool {
        self.inner.backing().can_export()
    }

    fn cpu_access(&self) -> CpuAccess {
        self.cpu_access
    }

    fn residency(&self) -> FrameResidency {
        self.inner.backing().residency()
    }

    fn dmabuf_plane(&self, index: usize) -> Option<DmabufPlane<'_>> {
        self.inner.backing().dmabuf_plane(index)
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        self.inner.backing().export_backing()
    }

    fn export_into(
        &self,
        out: &mut Vec<FrameFdPlane>,
    ) -> Result<Option<ExportedKind>, FrameExportError> {
        self.inner.export_into(out)
    }
}
