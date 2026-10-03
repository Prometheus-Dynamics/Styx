//! Capturing into buffers the caller provides.
//!
//! A consumer with its own fixed buffer pool (a PipeWire node: consumers map its buffers once,
//! when they are negotiated) hands those buffers to the capture with
//! [`StyxConfig::capture_into`](super::StyxConfig::capture_into) or
//! [`FramePlan::capture_into`](crate::planner::FramePlan::capture_into); the camera then writes
//! frames straight into them and frame N is in buffer [`CaptureBuffers::index_of`]. A buffer goes
//! back to the camera only when the frame in it is dropped, so a consumer keeps the frame while
//! its own user reads the buffer.
//!
//! Backends that import: V4L2 (`V4L2_MEMORY_DMABUF`, for dma-buf buffers, when the driver takes
//! them and lays the frame out as asked) and the virtual camera (any buffer). Elsewhere, or when
//! importing fails, the capture uses its own buffers as before and [`CaptureBuffers::in_use`]
//! stays false: the caller copies.

use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use smallvec::SmallVec;
use styx_core::prelude::*;

/// One buffer for [`CaptureBuffers::new`].
pub struct CaptureBuffer {
    /// A memfd or a dma-buf.
    pub fd: OwnedFd,
    /// Bytes the buffer holds.
    pub len: usize,
    /// `fd` is a dma-buf (V4L2 imports only dma-bufs).
    pub dmabuf: bool,
}

/// Buffers for a capture to fill: one frame of one format and layout per buffer. Cloning shares
/// them.
#[derive(Clone)]
pub struct CaptureBuffers {
    inner: Arc<Inner>,
}

struct Inner {
    format: MediaFormat,
    planes: SmallVec<[PlaneLayout; 3]>,
    slots: Vec<Slot>,
    in_use: AtomicBool,
}

struct Slot {
    buffer: CaptureBuffer,
    id: (u64, u64),
    map: NonNull<u8>,
}

// SAFETY: `map` is a shared read-only mapping owned by the slot, unmapped once in `Drop`; it is
// only read through shared slices.
unsafe impl Send for Slot {}
// SAFETY: see `Send`.
unsafe impl Sync for Slot {}

impl Drop for Slot {
    fn drop(&mut self) {
        // SAFETY: unmaps the mapping made in `CaptureBuffers::new`, once.
        unsafe {
            libc::munmap(self.map.as_ptr().cast(), self.buffer.len);
        }
    }
}

/// Device and inode of the file behind `fd`: equal for descriptors of one buffer.
pub(crate) fn identity(fd: BorrowedFd<'_>) -> io::Result<(u64, u64)> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fstat` fills the buffer for a valid descriptor; it is read only on success.
    if unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fstat` succeeded.
    let st = unsafe { st.assume_init() };
    #[allow(clippy::unnecessary_cast)]
    Ok((st.st_dev as u64, st.st_ino as u64))
}

impl CaptureBuffers {
    /// Buffers for frames of `format` laid out as `planes` (offsets within each buffer); every
    /// buffer must hold the planes.
    pub fn new(
        format: MediaFormat,
        planes: impl IntoIterator<Item = PlaneLayout>,
        buffers: Vec<CaptureBuffer>,
    ) -> io::Result<Self> {
        let planes: SmallVec<[PlaneLayout; 3]> = planes.into_iter().collect();
        let need = planes.iter().map(|p| p.offset + p.len).max().unwrap_or(0);
        if need == 0 || buffers.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no planes or buffers",
            ));
        }
        let mut slots = Vec::with_capacity(buffers.len());
        for buffer in buffers {
            if buffer.len < need {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("buffer of {} bytes for frames of {need}", buffer.len),
                ));
            }
            let id = identity(buffer.fd.as_fd())?;
            // SAFETY: a fresh read-only shared mapping of a descriptor we own; checked below.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    buffer.len,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    buffer.fd.as_raw_fd(),
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            let map = NonNull::new(ptr.cast::<u8>()).ok_or_else(|| io::Error::other("mmap"))?;
            slots.push(Slot { buffer, id, map });
        }
        Ok(Self {
            inner: Arc::new(Inner {
                format,
                planes,
                slots,
                in_use: AtomicBool::new(false),
            }),
        })
    }

    pub fn len(&self) -> usize {
        self.inner.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.slots.is_empty()
    }

    /// The format of the frames the buffers are for.
    pub fn format(&self) -> MediaFormat {
        self.inner.format
    }

    /// Where each plane of a frame lies in a buffer.
    pub fn planes(&self) -> &[PlaneLayout] {
        &self.inner.planes
    }

    /// Whether a capture fills these buffers now: a buffer whose frame is not held may be
    /// queued to the camera, so the caller must not write to it.
    pub fn in_use(&self) -> bool {
        self.inner.in_use.load(Ordering::Acquire)
    }

    /// The buffer `frame` lies in, if it is one of these.
    pub fn index_of(&self, frame: &FrameLease) -> Option<usize> {
        let id = match frame.export_backing().ok()? {
            FrameBackingExport::Memfd { fd, .. } => identity(fd.as_fd()).ok()?,
            FrameBackingExport::DmabufPlanes { planes } => {
                identity(planes.first()?.fd.as_fd()).ok()?
            }
        };
        self.inner.slots.iter().position(|slot| slot.id == id)
    }

    /// Mark the buffers as filled by a capture until the claim drops; `None` if another capture
    /// has them.
    pub(crate) fn claim(&self) -> Option<Claim> {
        self.inner
            .in_use
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Claim(self.clone()))
    }

    /// Buffer `index`'s descriptor.
    pub fn fd(&self, index: usize) -> Option<BorrowedFd<'_>> {
        self.inner.slots.get(index).map(|s| s.buffer.fd.as_fd())
    }

    /// Whether buffer `index` is a dma-buf.
    pub fn is_dmabuf(&self, index: usize) -> bool {
        self.inner.slots.get(index).is_some_and(|s| s.buffer.dmabuf)
    }

    /// The bytes of buffer `index`.
    pub(crate) fn bytes(&self, index: usize) -> Option<&[u8]> {
        let slot = self.inner.slots.get(index)?;
        // SAFETY: `map` maps `len` readable bytes for as long as the slot lives.
        Some(unsafe { std::slice::from_raw_parts(slot.map.as_ptr(), slot.buffer.len) })
    }

    /// A frame in buffer `index` laid out as `planes`; `release` runs when it is dropped (the
    /// capture takes the buffer back).
    pub(crate) fn frame(
        &self,
        index: usize,
        meta: FrameMeta,
        planes: SmallVec<[PlaneLayout; 3]>,
        release: impl FnOnce() + Send + 'static,
    ) -> FrameLease {
        let backing = Arc::new(Imported {
            buffers: self.clone(),
            index,
            release: Mutex::new(Some(Box::new(release))),
        });
        FrameLease::from_external(meta, planes, backing)
    }
}

impl fmt::Debug for CaptureBuffers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureBuffers")
            .field("format", &self.inner.format)
            .field("buffers", &self.inner.slots.len())
            .field("in_use", &self.in_use())
            .finish()
    }
}

/// The buffers are the camera's while this lives.
pub(crate) struct Claim(CaptureBuffers);

impl Claim {
    pub(crate) fn buffers(&self) -> &CaptureBuffers {
        &self.0
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.0.inner.in_use.store(false, Ordering::Release);
    }
}

type Release = Box<dyn FnOnce() + Send>;

/// A frame in one of the buffers.
struct Imported {
    buffers: CaptureBuffers,
    index: usize,
    release: Mutex<Option<Release>>,
}

impl ExternalBacking for Imported {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        // Every plane lies in the one buffer; plane layouts carry the offsets.
        (index < self.buffers.planes().len().max(1))
            .then(|| self.buffers.bytes(self.index))
            .flatten()
    }

    fn backing_bytes(&self) -> Option<usize> {
        self.buffers.bytes(self.index).map(<[u8]>::len)
    }

    fn backing_kind(&self) -> &'static str {
        "imported"
    }

    fn can_export(&self) -> bool {
        true
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        let slot = &self.buffers.inner.slots[self.index];
        let fd = slot.buffer.fd.try_clone().map_err(FrameExportError::Fd)?;
        let len = slot.buffer.len;
        Ok(Some(if slot.buffer.dmabuf {
            FrameBackingExport::DmabufPlanes {
                planes: vec![FrameFdPlane { fd, offset: 0, len }],
            }
        } else {
            FrameBackingExport::Memfd { fd, len }
        }))
    }
}

impl Drop for Imported {
    fn drop(&mut self) {
        if let Some(release) = self.release.get_mut().take() {
            release();
        }
    }
}

impl super::StyxConfig {
    /// Capture into `buffers` where the backend can (see [the module](self)); the frames' format
    /// and layout must be the buffers'. Without it, or where importing fails, the capture uses
    /// its own buffers.
    pub fn capture_into(mut self, buffers: CaptureBuffers) -> Self {
        self.capture_buffers = Some(buffers);
        self
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn memfd(len: usize) -> OwnedFd {
        use std::os::fd::FromRawFd;
        // SAFETY: memfd_create returns a new descriptor or -1 (checked).
        let fd = unsafe { libc::memfd_create(c"styx-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        // SAFETY: `fd` is a fresh descriptor we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: resizes the memfd we own.
        assert_eq!(
            unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) },
            0
        );
        fd
    }

    pub(crate) fn buffers(count: usize, width: u32, height: u32) -> CaptureBuffers {
        let format = MediaFormat::new(
            FourCc::YUYV,
            Resolution::new(width, height).unwrap(),
            ColorSpace::Srgb,
        );
        let len = (width * 2 * height) as usize;
        let planes = [PlaneLayout {
            offset: 0,
            len,
            stride: width as usize * 2,
        }];
        let list = (0..count)
            .map(|_| CaptureBuffer {
                fd: memfd(len),
                len,
                dmabuf: false,
            })
            .collect();
        CaptureBuffers::new(format, planes, list).unwrap()
    }

    #[test]
    fn frames_name_their_buffer_and_release_it_when_dropped() {
        let buffers = buffers(3, 8, 2);
        let claim = buffers.claim().expect("free");
        assert!(buffers.in_use());
        assert!(buffers.claim().is_none());
        let (tx, rx) = std::sync::mpsc::channel();
        let frame = buffers.frame(
            2,
            FrameMeta::new(buffers.format(), 0),
            buffers.planes().iter().copied().collect(),
            move || tx.send(2).unwrap(),
        );
        assert_eq!(buffers.index_of(&frame), Some(2));
        assert_eq!(frame.planes()[0].data().len(), 32);
        assert!(rx.try_recv().is_err());
        drop(frame);
        assert_eq!(rx.try_recv(), Ok(2));
        drop(claim);
        assert!(!buffers.in_use());
        let other = super::tests::buffers(1, 8, 2);
        let foreign = other.frame(
            0,
            FrameMeta::new(other.format(), 0),
            other.planes().iter().copied().collect(),
            || {},
        );
        assert_eq!(buffers.index_of(&foreign), None);
    }

    #[test]
    fn rejects_buffers_too_small_for_a_frame() {
        let format = MediaFormat::new(
            FourCc::GREY,
            Resolution::new(4, 4).unwrap(),
            ColorSpace::Srgb,
        );
        let planes = [PlaneLayout {
            offset: 0,
            len: 16,
            stride: 4,
        }];
        let small = vec![CaptureBuffer {
            fd: memfd(8),
            len: 8,
            dmabuf: false,
        }];
        assert!(CaptureBuffers::new(format, planes, small).is_err());
    }

    #[test]
    fn virtual_camera_captures_into_the_buffers_and_waits_for_them() {
        use crate::planner::plan_frames;
        use std::time::Duration;
        let device = crate::capture_api::CaptureRequest::virtual_source(
            crate::prelude::VirtualSourceConfig::new()
                .name("virtual")
                .format(FourCc::YUYV)
                .resolution(8, 2)
                .fps(100),
        )
        .into_device();
        let buffers = buffers(3, 8, 2);
        let plan = plan_frames(&device, &crate::planner::Frames::formats([FourCc::YUYV]))
            .unwrap()
            .capture_into(buffers.clone());
        let mut frames = plan.start().unwrap();
        let mut held = Vec::new();
        while held.len() < 3 {
            if let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_secs(2)) {
                held.push(frame);
            }
        }
        assert!(buffers.in_use());
        let mut indices: Vec<_> = held.iter().map(|f| buffers.index_of(f).unwrap()).collect();
        indices.sort_unstable();
        assert_eq!(indices, [0, 1, 2]);
        // Every buffer is held: the camera has nowhere to put a frame.
        assert!(matches!(
            frames.next_frame(Duration::from_millis(100)),
            RecvOutcome::Empty
        ));
        let returned = buffers.index_of(&held.remove(1)).unwrap();
        let RecvOutcome::Data(next) = frames.next_frame(Duration::from_secs(2)) else {
            panic!("no frame after a buffer came back");
        };
        assert_eq!(buffers.index_of(&next), Some(returned));
        frames.stop();
        drop((held, next));
        assert!(!buffers.in_use());
    }
}
