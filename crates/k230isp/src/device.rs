//! The vendor driver's `vvcam-isp` misc device (feature `device`, Linux): register reads and
//! writes through its ioctls, and the [`RegisterAccess`] the builder applies through.
//!
//! ```text
//! /dev/vvcam-isp.0  (misc device, ioctl magic 'v')
//!   VVCAM_ISP_READ_REG   _IOWR('v', 0x02, vvcam_isp_reg_t)   { addr, value }
//!   VVCAM_ISP_WRITE_REG  _IOW ('v', 0x03, vvcam_isp_reg_t)
//!   VVCAM_ISP_RESET      _IOW ('v', 0x01, u32)
//! ```
//!
//! The request numbers are the `_IOC` encodings of `vvcam_isp.h` (MIT, VeriSilicon), spelled
//! out. The driver maps the register window and reads or writes at `base + addr`, with no
//! range check of its own, so the addresses this crate passes are the ones in [`crate::regs`].
//! `mmap` of the window is not used: a read or write per register is enough for a parameter
//! set of a few hundred writes per frame. Nothing here has run on a board yet (written,
//! untested on board).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

use crate::params::RegisterAccess;
use crate::stats::{self, Raw};

/// The misc device of the vendor driver's ISP.
pub const DEVICE_PATH: &str = "/dev/vvcam-isp.0";

/// `VVCAM_ISP_RESET`: `_IOW('v', 0x01, uint32_t)`, a pointer to the reset value.
pub const VVCAM_ISP_RESET: u32 = 0x4004_7601;
/// `VVCAM_ISP_READ_REG`: `_IOWR('v', 0x02, vvcam_isp_reg_t)` (8 bytes).
pub const VVCAM_ISP_READ_REG: u32 = 0xC008_7602;
/// `VVCAM_ISP_WRITE_REG`: `_IOW('v', 0x03, vvcam_isp_reg_t)`.
pub const VVCAM_ISP_WRITE_REG: u32 = 0x4008_7603;
/// `VVCAM_ISP_CLOSE_IRQ`: `_IOW('v', 0x07, uint32_t)`. Not used by the builder.
pub const VVCAM_ISP_CLOSE_IRQ: u32 = 0x4004_7607;
/// `VVCAM_ISP_FE_START`: `_IOW('v', 0x08, isp_fe_buffer_t)`, for the K230's front end (not
/// used: the front end has no layout here).
pub const VVCAM_ISP_FE_START: u32 = 0x4010_7608;

/// `vvcam_isp_reg_t`: one register's address and value.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RegPair {
    addr: u32,
    value: u32,
}

/// An open `vvcam-isp` device.
#[derive(Debug)]
pub struct IspDevice {
    file: File,
}

impl IspDevice {
    /// Opens the device at `path` (normally [`DEVICE_PATH`]) for reading and writing.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self { file })
    }

    /// Resets the ISP with the driver's reset value.
    pub fn reset(&self, value: u32) -> io::Result<()> {
        let mut v = value;
        self.ioctl(VVCAM_ISP_RESET, &mut v)
    }

    /// Reads a statistics snapshot ([`stats::read_raw`]).
    pub fn statistics(&mut self) -> io::Result<Raw> {
        stats::read_raw(self)
    }

    fn ioctl<T>(&self, request: u32, arg: &mut T) -> io::Result<()> {
        // SAFETY: `arg` is a live `T` for the whole call, and each request's argument is
        // a `T` of the size its `_IOC` encodes (the register pair, or a `u32`), which the
        // driver copies in and, for `_IOWR`, back out.
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                request as _,
                core::ptr::from_mut(arg),
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl RegisterAccess for IspDevice {
    type Error = io::Error;

    fn read(&mut self, addr: u32) -> io::Result<u32> {
        let mut pair = RegPair { addr, value: 0 };
        self.ioctl(VVCAM_ISP_READ_REG, &mut pair)?;
        Ok(pair.value)
    }

    fn write(&mut self, addr: u32, value: u32) -> io::Result<()> {
        let mut pair = RegPair { addr, value };
        self.ioctl(VVCAM_ISP_WRITE_REG, &mut pair)
    }
}
