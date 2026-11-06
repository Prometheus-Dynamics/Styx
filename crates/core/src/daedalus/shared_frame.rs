//! Daedalus frames as Styx frames: a `daedalus:frame` handle ([`SharedFrame`]) becomes a
//! [`FrameLease`] without a copy ([`from_shared_frame`]), for Styx consumers fed by a graph,
//! including frames a separately built plugin made.
//!
//! - The lease reads the frame in place: its planes through the frame's own `plane_data`, its
//!   dma-bufs borrowed (valid while the lease lives; dup one to keep it longer, as for any
//!   lease), geometry, format, timestamp, sequence, residency, CPU access and colour from the
//!   frame. It is read-only, as Daedalus frames are. Capture metadata (exposure, gains) is not
//!   carried: a lease keeps it per backend, and a foreign frame has none of Styx's.
//! - One frame, one lease: the same frame gives the same `Arc` while that lease lives, so
//!   consumers that key caches on `Weak<FrameLease>` see one frame once. A Styx frame that went
//!   into a graph shared ([`super::shared_frame_payload`]) comes back as the very `Arc` it went
//!   in as.
//! - The frame is released exactly once, when the last clone of the lease drops: the lease holds
//!   one retain of it, whoever holds the lease (a node's state, a queue, a plugin unloaded
//!   meanwhile: a plugin's frames keep its library loaded).

use ::daedalus::transport::{
    ForeignOwner, FrameColorimetry, FrameFormatKind, FrameResidency as ViewResidency, FrameSource,
    PlaneMapping, SharedFrame,
};
use smallvec::SmallVec;

use crate::buffer::{
    BackingPool, CpuAccess, ExternalBacking, FrameLease, FrameMeta, FrameMutability,
    FrameResidency, PlaneLayout,
};
use crate::format::drm;
use crate::format::{ColorSpace, FourCc, MediaFormat, Resolution};
use crate::sync::{Arc, Mutex, Weak};

/// Planes a frame may have (Styx formats have at most 3; one spare).
pub const MAX_PLANES: usize = 4;

/// Why a frame cannot be a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharedFrameError {
    /// No Styx format for the frame's code (`format`, `modifier` and kind as the frame says).
    UnknownFormat {
        format: u32,
        modifier: u64,
        kind: FrameFormatKind,
    },
    /// Zero width or height.
    Empty,
    /// More planes than [`MAX_PLANES`], or none.
    Planes(u32),
}

impl core::fmt::Display for SharedFrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownFormat {
                format,
                modifier,
                kind,
            } => write!(
                f,
                "no Styx format for {kind:?} frame format {format:#010x} modifier {modifier:#x}"
            ),
            Self::Empty => f.write_str("frame has no pixels"),
            Self::Planes(n) => write!(f, "frame has {n} planes (1 to {MAX_PLANES} supported)"),
        }
    }
}

impl std::error::Error for SharedFrameError {}

/// Leases made here over foreign frames, by the frame's data pointer: the same frame finds its
/// lease while it lives. A live entry's pointer cannot be reused (the lease keeps the frame), and
/// dead ones are dropped before each new entry, so the list holds the frames alive and keeps its
/// allocation. Frames Styx sent are never here ([`sent_lease`] finds them without a reference),
/// so a sender's pooled frames stay unique (`Arc::get_mut`) once the graph lets go of them.
static LEASES: Mutex<Vec<(usize, Weak<FrameLease>)>> = Mutex::new(Vec::new());

fn remember(key: usize, lease: &Arc<FrameLease>) {
    let mut leases = LEASES.lock();
    leases.retain(|(_, w)| w.strong_count() > 0);
    leases.push((key, Arc::downgrade(lease)));
}

fn find(key: usize) -> Option<Arc<FrameLease>> {
    let leases = LEASES.lock();
    leases
        .iter()
        .filter(|(k, _)| *k == key)
        .find_map(|(_, w)| w.upgrade())
}

/// Entries for `key` in the list of leases over foreign frames, live or not yet swept (tests).
#[cfg(test)]
pub(super) fn entries_for(key: usize) -> usize {
    LEASES.lock().iter().filter(|(k, _)| *k == key).count()
}

/// The release function of a handle that owns an `Arc<FrameLease>` (`ForeignOwner::from_arc`
/// for `FrameLease`, which every Styx frame shared through Daedalus goes through), as an
/// address, once a lease has been seen. Its code drops a `FrameLease`, so no other type's owner
/// has it; a second copy of it elsewhere in the build would only make [`sent_lease`] miss.
static LEASE_RELEASE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

fn note_release(sample: &Arc<FrameLease>) {
    LEASE_RELEASE.get_or_init(|| {
        let owner = ForeignOwner::from_arc(Arc::clone(sample));
        let release = owner.release as usize;
        // SAFETY: `owner` took over the clone made above; this gives that reference back.
        unsafe { (owner.release)(owner.ptr) };
        release
    });
}

/// A Styx frame sent into a graph shared ([`super::shared_frame_payload`]). Nothing is recorded
/// per frame: [`from_shared_frame`] knows the frame's handle by its owner.
pub(super) fn remember_sent(frame: &Arc<FrameLease>) {
    note_release(frame);
}

/// The `Arc<FrameLease>` a handle owns, when it owns one and its data is that lease.
fn sent_lease(frame: &SharedFrame) -> Option<Arc<FrameLease>> {
    let owner = frame.handle().owner();
    if !frame.is_local() || owner.ptr != frame.handle().data() {
        return None;
    }
    if owner.release as usize != *LEASE_RELEASE.get()? {
        return None;
    }
    let ptr = owner.ptr.cast::<FrameLease>();
    // SAFETY: the owner's release is `ForeignOwner::from_arc::<FrameLease>`'s, so `owner.ptr`
    // came from `Arc::into_raw` of an `Arc<FrameLease>`, and `frame` holds one strong reference
    // of it while it lives: adding one is a clone of a live `Arc`.
    unsafe {
        Arc::increment_strong_count(ptr);
        Some(Arc::from_raw(ptr))
    }
}

/// `frame` as a [`SharedFrame`], made here so that [`from_shared_frame`] gives this `Arc` back
/// (`SharedFrame::new` called from another crate works too, but its handle's owner code is that
/// crate's copy, so in an optimised build the frame comes back as a new lease over it). Nothing
/// is copied or kept.
pub fn share_frame(frame: Arc<FrameLease>) -> SharedFrame {
    note_release(&frame);
    SharedFrame::new(frame)
}

/// `frame` as a [`FrameLease`] (see the [module documentation](self)): the frame's own `Arc`
/// when it is a Styx frame, the lease already made for it if one lives, else a new one over a
/// retain of it. No pixels are copied or mapped here.
pub fn from_shared_frame(frame: &SharedFrame) -> Result<Arc<FrameLease>, SharedFrameError> {
    if let Some(lease) = sent_lease(frame) {
        return Ok(lease);
    }
    let key = frame.handle().data() as usize;
    if let Some(lease) = find(key) {
        return Ok(lease);
    }
    let lease = Arc::new(lease_over(frame.clone())?);
    note_release(&lease);
    remember(key, &lease);
    Ok(lease)
}

/// The Styx code for the frame's format.
fn fourcc(frame: &SharedFrame) -> Result<FourCc, SharedFrameError> {
    let (format, modifier, kind) = (frame.format(), frame.modifier(), frame.format_kind());
    let code = match kind {
        FrameFormatKind::Compressed => Some(FourCc::new(format.to_le_bytes())),
        _ => drm::from_drm(format, modifier),
    };
    code.ok_or(SharedFrameError::UnknownFormat {
        format,
        modifier,
        kind,
    })
}

/// The Styx label for a frame's colour, by primaries and transfer (Styx's spaces carry no
/// range or matrix of their own; see [`super::colorimetry`] for the other way).
pub fn color_space(c: FrameColorimetry) -> ColorSpace {
    const P709: u8 = FrameColorimetry::PRIMARIES_BT709;
    match (c.primaries, c.transfer) {
        (P709, FrameColorimetry::TRANSFER_SRGB) => ColorSpace::Srgb,
        (P709, FrameColorimetry::TRANSFER_BT709) => ColorSpace::Bt709,
        (FrameColorimetry::PRIMARIES_BT2020, _) => ColorSpace::Bt2020,
        _ => ColorSpace::Unknown,
    }
}

fn lease_over(frame: SharedFrame) -> Result<FrameLease, SharedFrameError> {
    let code = fourcc(&frame)?;
    let res = Resolution::new(frame.width(), frame.height()).ok_or(SharedFrameError::Empty)?;
    let count = frame.plane_count();
    if count == 0 || count as usize > MAX_PLANES {
        return Err(SharedFrameError::Planes(count));
    }
    let mut layouts: SmallVec<[PlaneLayout; 3]> = SmallVec::new();
    let mut dmabuf = false;
    for i in 0..count {
        let p = frame.plane(i).ok_or(SharedFrameError::Planes(count))?;
        dmabuf |= p.dmabuf_fd.is_some();
        // `plane_data(i)` is the plane itself, from its first byte.
        layouts.push(PlaneLayout {
            offset: 0,
            len: p.len as usize,
            stride: p.stride as usize,
        });
    }
    let residency = match frame.residency() {
        ViewResidency::Cpu => FrameResidency::HostExternal,
        ViewResidency::Gpu => FrameResidency::GpuTexture,
        ViewResidency::External if dmabuf => FrameResidency::Dmabuf,
        ViewResidency::External => FrameResidency::HostExternal,
    };
    let access = match (residency, frame.plane(0).map(|p| p.mapping)) {
        (FrameResidency::GpuTexture, _) | (_, Some(PlaneMapping::Unmapped) | None) => {
            CpuAccess::None
        }
        (_, Some(PlaneMapping::Cached)) => CpuAccess::Cached,
        (_, Some(_)) => CpuAccess::Uncached,
    };
    let format = MediaFormat::new(code, res, color_space(frame.colorimetry()));
    let mut meta = FrameMeta::new(format, frame.timestamp_ns())
        .with_residency(residency)
        .with_mutability(FrameMutability::ReadOnly);
    let sequence = frame.sequence();
    if sequence > 0 {
        meta.hops.set_sequence(Some(sequence as u32));
    }
    let backing = SharedFrameBacking {
        frame,
        residency,
        access,
        bytes: layouts.iter().map(|l| l.len).sum(),
        opened: Default::default(),
    };
    Ok(FrameLease::from_external(
        meta,
        layouts,
        pool().wrap(backing),
    ))
}

/// Backing records reused across frames (frames in flight at most).
fn pool() -> &'static BackingPool<SharedFrameBacking> {
    static POOL: std::sync::OnceLock<BackingPool<SharedFrameBacking>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| BackingPool::new(16))
}

/// A lease's memory: one retain of the frame, released when the record goes.
struct SharedFrameBacking {
    frame: SharedFrame,
    residency: FrameResidency,
    access: CpuAccess,
    bytes: usize,
    /// Planes read through [`ExternalBacking::plane_data`] (unbracketed): the address and
    /// length the frame gave, its CPU access left open until the record drops.
    opened: [std::sync::OnceLock<Option<(usize, usize)>>; MAX_PLANES],
}

impl ExternalBacking for SharedFrameBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        if self.access == CpuAccess::None {
            return None;
        }
        let slot = self.opened.get(index)?;
        let &(ptr, len) = slot
            .get_or_init(|| {
                self.frame
                    .plane_data(index as u32)
                    .map(|b| (b.as_ptr() as usize, b.len()))
            })
            .as_ref()?;
        // SAFETY: the frame gave these bytes and keeps them valid until the plane's
        // `end_cpu_access`, which only `Drop` calls, after every borrow of `self` has ended.
        Some(unsafe { core::slice::from_raw_parts(ptr as *const u8, len) })
    }

    fn begin_cpu_read(&self, index: usize) -> Option<&[u8]> {
        if self.access == CpuAccess::None {
            return None;
        }
        self.frame.plane_data(index as u32)
    }

    fn end_cpu_read(&self, index: usize) {
        self.frame.end_cpu_access(index as u32);
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.bytes)
    }

    fn backing_kind(&self) -> &'static str {
        "daedalus-frame"
    }

    fn residency(&self) -> FrameResidency {
        self.residency
    }

    fn cpu_access(&self) -> CpuAccess {
        self.access
    }

    #[cfg(unix)]
    fn dmabuf_plane(&self, index: usize) -> Option<crate::buffer::DmabufPlane<'_>> {
        let p = self.frame.plane(index as u32)?;
        let fd = p.dmabuf_fd.filter(|fd| *fd >= 0)?;
        Some(crate::buffer::DmabufPlane {
            // SAFETY: the frame's descriptors stay open while any retain of it lives, and
            // `self` holds one for the borrow's lifetime.
            fd: unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
            offset: p.offset as usize,
        })
    }
}

impl Drop for SharedFrameBacking {
    fn drop(&mut self) {
        for (i, slot) in self.opened.iter().enumerate() {
            if let Some(Some(_)) = slot.get() {
                self.frame.end_cpu_access(i as u32);
            }
        }
    }
}
