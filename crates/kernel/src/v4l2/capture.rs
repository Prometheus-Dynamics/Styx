//! Helpers for capture backends built on [`VideoDevice`]: waiting for a filled capture buffer
//! only, and exporting buffers as dma-bufs with chosen access.

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::time::Duration;

use super::raw;
use super::{BufType, VideoDevice};
use crate::{Error, Ready, Result, ioctl};

/// Access mode of an exported dma-buf descriptor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DmaBufAccess {
    /// `O_RDONLY`: importers can read (and map read-only).
    #[default]
    ReadOnly,
    /// `O_RDWR`: importers can also map it writable.
    ReadWrite,
}

impl DmaBufAccess {
    fn open_flags(self) -> u32 {
        let access = match self {
            DmaBufAccess::ReadOnly => libc::O_RDONLY,
            DmaBufAccess::ReadWrite => libc::O_RDWR,
        };
        (libc::O_CLOEXEC | access) as u32
    }
}

impl VideoDevice {
    /// Waits until a buffer can be dequeued from a capture queue (`POLLIN`), or the timeout
    /// passes (an empty [`Ready`]). `None` waits forever. `error`/`hangup` report a stopped
    /// queue or a device that went away.
    pub fn wait_readable(&self, timeout: Option<Duration>) -> Result<Ready> {
        ioctl::poll_fd(self.as_fd(), libc::POLLIN, timeout)
    }

    /// Exports one plane of an MMAP buffer as a close-on-exec dma-buf with the given access
    /// (`VIDIOC_EXPBUF`). [`VideoDevice::export_buffer`] is the read-write form.
    pub fn export_buffer_with(
        &self,
        buf_type: BufType,
        index: u32,
        plane: u32,
        access: DmaBufAccess,
    ) -> Result<OwnedFd> {
        let mut raw = raw::v4l2_exportbuffer {
            type_: buf_type.to_raw(),
            index,
            plane,
            flags: access.open_flags(),
            ..Default::default()
        };
        // SAFETY: VIDIOC_EXPBUF takes a `v4l2_exportbuffer`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_EXPBUF, &mut raw)? };
        if raw.fd < 0 {
            return Err(Error::Invalid("VIDIOC_EXPBUF returned no fd".into()));
        }
        // SAFETY: the kernel returned a new dma-buf descriptor that we now own.
        Ok(unsafe { OwnedFd::from_raw_fd(raw.fd) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_flags_match_open_flags() {
        let ro = DmaBufAccess::ReadOnly.open_flags();
        assert_eq!(ro & libc::O_ACCMODE as u32, libc::O_RDONLY as u32);
        assert_ne!(ro & libc::O_CLOEXEC as u32, 0);
        let rw = DmaBufAccess::ReadWrite.open_flags();
        assert_eq!(rw & libc::O_ACCMODE as u32, libc::O_RDWR as u32);
        assert_eq!(DmaBufAccess::default(), DmaBufAccess::ReadOnly);
    }
}
