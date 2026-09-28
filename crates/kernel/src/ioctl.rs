//! ioctl number encoding (the kernel's `_IOC` macros), the raw ioctl call, and device opening.

use std::ffi::CString;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::{Error, Result};

#[cfg(any(
    target_arch = "powerpc",
    target_arch = "powerpc64",
    target_arch = "mips",
    target_arch = "mips64",
    target_arch = "sparc",
    target_arch = "sparc64"
))]
mod enc {
    pub const NONE: u32 = 1;
    pub const READ: u32 = 2;
    pub const WRITE: u32 = 4;
    pub const SIZE_BITS: u32 = 13;
}

#[cfg(not(any(
    target_arch = "powerpc",
    target_arch = "powerpc64",
    target_arch = "mips",
    target_arch = "mips64",
    target_arch = "sparc",
    target_arch = "sparc64"
)))]
mod enc {
    pub const NONE: u32 = 0;
    pub const WRITE: u32 = 1;
    pub const READ: u32 = 2;
    pub const SIZE_BITS: u32 = 14;
}

const NR_SHIFT: u32 = 0;
const TYPE_SHIFT: u32 = 8;
const SIZE_SHIFT: u32 = 16;
const DIR_SHIFT: u32 = SIZE_SHIFT + enc::SIZE_BITS;

/// `_IOC(dir, type, nr, size)`.
pub(crate) const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> u32 {
    assert!(size < (1 << enc::SIZE_BITS));
    (dir << DIR_SHIFT)
        | ((ty as u32) << TYPE_SHIFT)
        | ((nr as u32) << NR_SHIFT)
        | ((size as u32) << SIZE_SHIFT)
}

/// `_IO(type, nr)`.
pub(crate) const fn io(ty: u8, nr: u8) -> u32 {
    ioc(enc::NONE, ty, nr, 0)
}

/// `_IOR(type, nr, T)`.
pub(crate) const fn ior<T>(ty: u8, nr: u8) -> u32 {
    ioc(enc::READ, ty, nr, size_of::<T>())
}

/// `_IOW(type, nr, T)`.
pub(crate) const fn iow<T>(ty: u8, nr: u8) -> u32 {
    ioc(enc::WRITE, ty, nr, size_of::<T>())
}

/// `_IOWR(type, nr, T)`.
pub(crate) const fn iowr<T>(ty: u8, nr: u8) -> u32 {
    ioc(enc::READ | enc::WRITE, ty, nr, size_of::<T>())
}

/// An ioctl request: its number and its uAPI name for error messages.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ioctl {
    pub nr: u32,
    pub name: &'static str,
}

/// Declares `pub(crate) const NAME: Ioctl` values.
macro_rules! ioctls {
    ($($name:ident = $nr:expr;)*) => {
        $(
            #[allow(dead_code)]
            pub(crate) const $name: $crate::ioctl::Ioctl =
                $crate::ioctl::Ioctl { nr: $nr, name: stringify!($name) };
        )*
    };
}
pub(crate) use ioctls;

/// Calls `ioctl(fd, req, arg)`, retrying on `EINTR`.
///
/// # Safety
///
/// `arg` must point to a value of the type the ioctl expects (or be the integer argument for
/// ioctls that take one by value), valid for reads and writes for the duration of the call.
pub(crate) unsafe fn ioctl_raw(
    fd: BorrowedFd<'_>,
    req: Ioctl,
    arg: *mut libc::c_void,
) -> Result<libc::c_int> {
    loop {
        // SAFETY: the caller guarantees `arg` matches `req`; `fd` is a valid open descriptor.
        let ret = unsafe { libc::ioctl(fd.as_raw_fd(), req.nr as _, arg) };
        if ret >= 0 {
            return Ok(ret);
        }
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno != libc::EINTR {
            return Err(Error::ioctl(req.name, errno));
        }
    }
}

/// Calls an ioctl whose argument is a pointer to `T`.
///
/// # Safety
///
/// `T` must be the exact uAPI structure the ioctl `req` reads and writes (the ioctl number
/// encodes its size, which the size assertions next to each definition check).
pub(crate) unsafe fn ioctl<T>(fd: BorrowedFd<'_>, req: Ioctl, arg: &mut T) -> Result<libc::c_int> {
    // SAFETY: forwarded to the caller: `arg` is a live, exclusive `T` of the right type.
    unsafe { ioctl_raw(fd, req, (arg as *mut T).cast()) }
}

/// Opens a character device read-write, close-on-exec and (optionally) non-blocking.
pub(crate) fn open_device(path: &Path, nonblocking: bool) -> Result<OwnedFd> {
    open_path(path, libc::O_RDWR, nonblocking)
}

/// Opens `path` with `access` (`O_RDONLY`/`O_RDWR`), always close-on-exec.
pub(crate) fn open_path(path: &Path, access: libc::c_int, nonblocking: bool) -> Result<OwnedFd> {
    let cpath = CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::Open {
        path: path.to_owned(),
        source: std::io::Error::from_raw_os_error(libc::EINVAL),
    })?;
    let mut flags = access | libc::O_CLOEXEC;
    if nonblocking {
        flags |= libc::O_NONBLOCK;
    }
    loop {
        // SAFETY: `cpath` is a valid NUL-terminated string that outlives the call.
        let fd = unsafe { libc::open(cpath.as_ptr(), flags) };
        if fd >= 0 {
            // SAFETY: `open` returned a fresh descriptor that nothing else owns.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let source = std::io::Error::last_os_error();
        if source.raw_os_error() != Some(libc::EINTR) {
            return Err(Error::Open {
                path: path.to_owned(),
                source,
            });
        }
    }
}

/// Sets or clears `O_NONBLOCK` on a descriptor.
pub(crate) fn set_nonblocking(fd: BorrowedFd<'_>, nonblocking: bool) -> Result<()> {
    // SAFETY: F_GETFL on a valid descriptor has no memory effects.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(Error::sys("fcntl(F_GETFL)"));
    }
    let flags = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    // SAFETY: F_SETFL with plain flags on a valid descriptor.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags) } < 0 {
        return Err(Error::sys("fcntl(F_SETFL)"));
    }
    Ok(())
}

/// Readiness of a device descriptor, as reported by `poll(2)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ready {
    /// `POLLIN`: a capture buffer can be dequeued (or data read).
    pub readable: bool,
    /// `POLLOUT`: an output buffer can be dequeued.
    pub writable: bool,
    /// `POLLPRI`: a V4L2 event is pending, or a media request completed.
    pub priority: bool,
    /// `POLLERR`: the device reported an error (for example, streaming stopped).
    pub error: bool,
    /// `POLLHUP`: the device went away.
    pub hangup: bool,
}

impl Ready {
    /// True when nothing is ready (the wait timed out).
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn from_revents(revents: i16) -> Self {
        Self {
            readable: revents & libc::POLLIN != 0,
            writable: revents & libc::POLLOUT != 0,
            priority: revents & libc::POLLPRI != 0,
            error: revents & libc::POLLERR != 0,
            hangup: revents & libc::POLLHUP != 0,
        }
    }
}

/// Waits until `fd` has any of `events` (`POLLIN`, `POLLPRI`, ...) ready, or the timeout
/// passes (an empty [`Ready`]). `None` waits forever.
pub(crate) fn poll_fd(
    fd: BorrowedFd<'_>,
    events: i16,
    timeout: Option<std::time::Duration>,
) -> Result<Ready> {
    let timeout_ms = match timeout {
        None => -1,
        Some(t) => t.as_millis().min(i32::MAX as u128) as i32,
    };
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    };
    loop {
        // SAFETY: `pfd` is a single valid pollfd for the duration of the call.
        let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if ret >= 0 {
            return Ok(if ret == 0 {
                Ready::default()
            } else {
                Ready::from_revents(pfd.revents)
            });
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return Err(Error::Sys {
                call: "poll",
                source: err,
            });
        }
    }
}

/// Lists `/dev/<prefix>N` nodes (e.g. `video0`, `media3`), sorted by `N`.
pub(crate) fn list_dev_nodes(prefix: &str) -> Vec<std::path::PathBuf> {
    let mut found: Vec<(u32, std::path::PathBuf)> = std::fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let n = name.to_str()?.strip_prefix(prefix)?.parse::<u32>().ok()?;
            Some((n, entry.path()))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, p)| p).collect()
}

/// Converts a fixed-size, NUL-padded C string field to a `String` (lossy).
pub(crate) fn cstr_field(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn encodes_like_the_kernel_macros() {
        // Values from the C headers on x86_64/aarch64.
        assert_eq!(ior::<[u8; 104]>(b'V', 0), 0x8068_5600); // VIDIOC_QUERYCAP
        assert_eq!(iowr::<[u8; 208]>(b'V', 5), 0xc0d0_5605); // VIDIOC_S_FMT
        assert_eq!(iow::<libc::c_int>(b'V', 18), 0x4004_5612); // VIDIOC_STREAMON
        assert_eq!(io(b'|', 0x80), 0x7c80); // MEDIA_REQUEST_IOC_QUEUE
        assert_eq!(ior::<libc::c_int>(b'|', 0x05), 0x8004_7c05); // MEDIA_IOC_REQUEST_ALLOC
    }

    #[test]
    fn cstr_fields_stop_at_nul() {
        assert_eq!(cstr_field(b"uvcvideo\0\0\0junk"), "uvcvideo");
        assert_eq!(cstr_field(b"full"), "full");
    }
}
