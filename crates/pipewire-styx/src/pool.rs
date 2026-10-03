//! The node's own buffer pool: one memfd per PipeWire buffer, also as a dma-buf (`udmabuf`)
//! where the kernel allows it. PipeWire consumers map these buffers once; the camera captures
//! into them where it can ([`styx::capture_api::CaptureBuffers`]), else each frame is copied in.
//! No PipeWire types here, so this is tested without the PipeWire libraries.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;

use styx::capture_api::{CaptureBuffer, CaptureBuffers};
use styx::prelude::*;

/// One buffer: a sealed memfd, mapped writable, and the same pages as a dma-buf if possible.
pub struct Slot {
    pub memfd: OwnedFd,
    pub dmabuf: Option<OwnedFd>,
    pub len: usize,
    map: NonNull<u8>,
}

impl Slot {
    /// A buffer of at least `len` bytes (rounded up to pages); `dmabuf` also makes a dma-buf of
    /// it (none if `/dev/udmabuf` is not usable).
    pub fn new(len: usize, dmabuf: bool) -> io::Result<Self> {
        let len = len.max(1).next_multiple_of(page_size());
        // SAFETY: memfd_create returns a new descriptor or -1 (checked).
        let fd = unsafe {
            libc::memfd_create(
                c"styx-pipewire".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor we own.
        let memfd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: plain calls on the descriptor we own; results checked.
        unsafe {
            if libc::ftruncate(fd, len as libc::off_t) != 0
                || libc::fcntl(
                    fd,
                    libc::F_ADD_SEALS,
                    libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL,
                ) != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: a fresh shared mapping of the memfd; checked.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let map = NonNull::new(ptr.cast::<u8>()).ok_or_else(|| io::Error::other("mmap"))?;
        let dmabuf = if dmabuf {
            udmabuf(&memfd, len).ok()
        } else {
            None
        };
        Ok(Self {
            memfd,
            dmabuf,
            len,
            map,
        })
    }

    /// The buffer's bytes, to copy a frame in. Only while no capture fills the buffer.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `map` maps `len` writable bytes for as long as `self` lives.
        unsafe { std::slice::from_raw_parts_mut(self.map.as_ptr(), self.len) }
    }

    /// The mapping's address (PipeWire's `spa_data.data`).
    pub fn ptr(&self) -> *mut u8 {
        self.map.as_ptr()
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        // SAFETY: unmaps the mapping made in `new`, once.
        unsafe {
            libc::munmap(self.map.as_ptr().cast(), self.len);
        }
    }
}

fn page_size() -> usize {
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page).unwrap_or(4096).max(1)
}

/// `struct udmabuf_create` from `<linux/udmabuf.h>`.
#[repr(C)]
struct UdmabufCreate {
    memfd: u32,
    flags: u32,
    offset: u64,
    size: u64,
}

/// `UDMABUF_CREATE`: `_IOW('u', 0x42, struct udmabuf_create)`.
const UDMABUF_CREATE: libc::c_ulong =
    (1 << 30) | (24 << 16) | ((b'u' as libc::c_ulong) << 8) | 0x42;
const UDMABUF_FLAGS_CLOEXEC: u32 = 1;

/// A dma-buf of the (sealed, page-sized) memfd's pages.
fn udmabuf(memfd: &OwnedFd, len: usize) -> io::Result<OwnedFd> {
    let dev = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/udmabuf")?;
    let create = UdmabufCreate {
        memfd: memfd.as_raw_fd() as u32,
        flags: UDMABUF_FLAGS_CLOEXEC,
        offset: 0,
        size: len as u64,
    };
    // SAFETY: UDMABUF_CREATE reads the struct and returns a new descriptor or -1 (checked).
    let fd = unsafe { libc::ioctl(dev.as_raw_fd(), UDMABUF_CREATE as _, &create) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Whether slots can be dma-bufs here (`/dev/udmabuf` usable).
pub fn dmabuf_available() -> bool {
    Slot::new(1, true).is_ok_and(|slot| slot.dmabuf.is_some())
}

/// The slots as buffers a capture can fill with frames of `format` packed as `planes`: their
/// dma-bufs when every slot has one (V4L2 imports only those), else their memfds.
pub fn capture_buffers<'a>(
    format: MediaFormat,
    planes: Vec<PlaneLayout>,
    slots: impl IntoIterator<Item = &'a Slot> + Clone,
) -> io::Result<CaptureBuffers> {
    let dmabuf = slots.clone().into_iter().all(|s| s.dmabuf.is_some());
    let buffers = slots
        .into_iter()
        .map(|slot| {
            let fd = match (&slot.dmabuf, dmabuf) {
                (Some(fd), true) => fd.try_clone()?,
                _ => slot.memfd.try_clone()?,
            };
            Ok(CaptureBuffer {
                fd,
                len: slot.len,
                dmabuf,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    CaptureBuffers::new(format, planes, buffers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_page_sized_writable_memfds() {
        let mut slot = Slot::new(100, false).unwrap();
        assert_eq!(slot.len % page_size(), 0);
        slot.bytes_mut()[..3].copy_from_slice(b"abc");
        // The pages are the memfd's: a second mapping sees the bytes.
        let other = Slot {
            memfd: slot.memfd.try_clone().unwrap(),
            dmabuf: None,
            len: slot.len,
            // SAFETY: maps the memfd we hold; checked.
            map: NonNull::new(unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    slot.len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    slot.memfd.as_raw_fd(),
                    0,
                )
            })
            .unwrap()
            .cast(),
        };
        // SAFETY: `other` maps `len` bytes.
        let seen = unsafe { std::slice::from_raw_parts(other.ptr(), 3) };
        assert_eq!(seen, b"abc");
    }

    #[test]
    fn capture_buffers_name_the_slot_a_frame_is_in() {
        let format = MediaFormat::new(
            FourCc::YUYV,
            Resolution::new(8, 2).unwrap(),
            ColorSpace::Srgb,
        );
        let planes = vec![PlaneLayout {
            offset: 0,
            len: 32,
            stride: 16,
        }];
        let slots: Vec<Slot> = (0..2).map(|_| Slot::new(32, true).unwrap()).collect();
        let buffers = capture_buffers(format, planes, &slots).unwrap();
        assert_eq!(buffers.len(), 2);
        // A virtual camera "captures" into them; its frames name their slot.
        let device = CaptureRequest::virtual_source(
            VirtualSourceConfig::new()
                .name("virtual")
                .format(FourCc::YUYV)
                .resolution(8, 2)
                .fps(100),
        )
        .into_device();
        let mut frames =
            styx::planner::plan_frames(&device, &FrameRequirements::formats([FourCc::YUYV]))
                .unwrap()
                .capture_into(buffers.clone())
                .start()
                .unwrap();
        let frame = loop {
            if let RecvOutcome::Data(frame) = frames.next_frame(std::time::Duration::from_secs(2)) {
                break frame;
            }
        };
        assert!(buffers.in_use());
        assert!(buffers.index_of(&frame).is_some());
    }
}
