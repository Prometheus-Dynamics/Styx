//! Importing a received frame over its descriptors: the cached mappings, and the release the
//! server gets once the frame and its companions are dropped.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use smallvec::SmallVec;
use styx_core::buffer::FrameProgress;
use styx_core::prelude::*;

use super::super::mapcache::{CachedDmabuf, Fds, Inner, MapCache};
use super::super::progress_ring::{ProgressRing, SlotSource};
use super::super::wire::{self, ClientHops, NO_OUTPUT, WireBacking, WireFrame, WireProgress};
use super::super::{IpcError, socket};

/// The progress page sliced frames came with on the current connection (`progress_ring.rs`),
/// keyed by the connection's generation (a socket's address can be reused after a reconnect).
#[derive(Default)]
pub(super) struct Rings(Mutex<Option<(u64, Arc<ProgressRing>)>>);

/// A sliced frame's progress, as the frame message gave it.
pub(super) struct Sliced {
    ring: Arc<ProgressRing>,
    progress: WireProgress,
    id: u64,
}

impl Rings {
    /// The progress of frame `id` received on connection `generation`, when it has a trailer:
    /// its page is the last of `fds` with the connection's first sliced frame, its slot names
    /// where to read.
    pub(super) fn take(
        &self,
        generation: u64,
        progress: Option<&WireProgress>,
        fds: &mut Vec<OwnedFd>,
        id: u64,
    ) -> Result<Option<Sliced>, IpcError> {
        let Some(progress) = progress else {
            return Ok(None);
        };
        let key = generation;
        let mut rings = self.0.lock();
        if progress.ring_attached {
            let fd = fds
                .pop()
                .ok_or(IpcError::Malformed("progress page missing"))?;
            *rings = Some((key, Arc::new(ProgressRing::from_fd(fd)?)));
        }
        match rings.as_ref() {
            Some((k, ring)) if *k == key => Ok(Some(Sliced {
                ring: ring.clone(),
                progress: progress.clone(),
                id,
            })),
            // Without its page the frame's rows cannot be known: not read before complete.
            _ => Err(IpcError::Malformed(
                "sliced frame without its progress page",
            )),
        }
    }
}

/// `frame` over the descriptors `fds` yields, released on the server with `release` once it
/// and its companions are dropped. Its hops get the receive (from `release`) and import times.
pub(super) fn import(
    frame: &WireFrame,
    fds: &mut impl Iterator<Item = OwnedFd>,
    mut release: Release,
    maps: &Arc<MapCache>,
    sliced: Option<Sliced>,
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
    // A sliced frame: its leases share progress read from the service's page; once complete,
    // the dma-bufs are synced for the CPU again (rows below the first slice may have been
    // cached before the service's ISP wrote them).
    // (Only sliced frames allocate for this: other frames keep to the one allocation.)
    let resync: Option<Arc<OnceLock<Vec<OwnedFd>>>> = sliced.as_ref().map(|_| Arc::default());
    let progress = sliced.as_ref().map(|s| {
        let fds = resync.clone().unwrap_or_default();
        let source = SlotSource::new(
            s.ring.clone(),
            usize::from(s.progress.slot),
            s.id,
            s.progress.heights,
            Box::new(move || {
                for fd in fds.get().into_iter().flatten() {
                    let _ = styx_core::buffer::dmabuf_end_cpu_read(fd.as_raw_fd());
                    let _ = styx_core::buffer::dmabuf_begin_cpu_read(fd.as_raw_fd());
                }
            }),
        );
        Arc::new(FrameProgress::with_source(Arc::new(source)))
    });
    let lease_progress = |i: usize| {
        let (p, s) = (progress.as_ref()?, sliced.as_ref()?);
        let o = *s.progress.outputs.get(i)?;
        (o != NO_OUTPUT).then(|| (p.clone(), usize::from(o)))
    };
    let mut synced = Vec::new();
    let (mut out, dmabuf) = import_part(frame, meta, fds, handle, maps, lease_progress(0))?;
    synced.extend(dmabuf);
    for (i, (kind, companion)) in frame.companions.iter().enumerate() {
        let release = ReleaseHandle::Shared(shared.clone().expect("shared with companions"));
        let (part, dmabuf) = import_part(
            companion,
            companion.meta.clone(),
            fds,
            release,
            maps,
            lease_progress(i + 1),
        )?;
        synced.extend(dmabuf);
        out = out
            .with_companion(*kind, part)
            .map_err(|_| IpcError::Malformed("companion does not match its frame"))?;
    }
    if let Some(resync) = resync {
        let _ = resync.set(synced);
    }
    Ok(out)
}

fn import_part(
    frame: &WireFrame,
    mut meta: FrameMeta,
    fds: &mut impl Iterator<Item = OwnedFd>,
    release: ReleaseHandle,
    maps: &Arc<MapCache>,
    progress: Option<(Arc<FrameProgress>, usize)>,
) -> Result<(FrameLease, Option<OwnedFd>), IpcError> {
    let count = frame.backing.fd_count();
    let own: Fds = fds.by_ref().take(count).collect();
    if own.len() != count {
        return Err(IpcError::Malformed("descriptors missing"));
    }
    // A sliced frame's dma-buf, to sync again once the frame is complete.
    let dmabuf = match (&progress, &frame.backing) {
        (Some(_), WireBacking::Dmabuf(_)) => own.first().and_then(|fd| fd.try_clone().ok()),
        _ => None,
    };
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
    let lease = FrameLease::from_external(
        meta,
        layouts,
        Arc::new(Released {
            inner,
            _release: release,
            cpu_access: frame.cpu_access,
            progress,
        }),
    );
    Ok((lease, dmabuf))
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
    /// A sliced frame's progress (shared by its leases) and this lease's output in it.
    progress: Option<(Arc<FrameProgress>, usize)>,
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

    fn host_spans(&self, span: &mut dyn FnMut(usize, usize)) -> bool {
        self.inner.backing().host_spans(span)
    }

    fn progress(&self) -> Option<FrameProgressRef<'_>> {
        self.progress.as_ref().map(|(p, i)| p.output(*i))
    }

    fn progress_handle(&self) -> Option<styx_core::buffer::FrameProgressHandle> {
        let (p, i) = self.progress.as_ref()?;
        Some(styx_core::buffer::FrameProgressHandle::new(p.clone(), *i))
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
