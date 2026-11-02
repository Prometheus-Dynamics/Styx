//! USB devices through usbfs (`/dev/bus/usb/BBB/DDD`): descriptors, control and bulk
//! transfers, interface claiming (detaching a kernel driver from one interface on request),
//! alternate settings, and asynchronous isochronous/bulk transfers ([`UrbRing`]).
//!
//! This is what a userspace USB driver needs (the userspace UVC backend, `styx-uvc`), with no
//! libusb: the `USBDEVFS_*` ioctls from `<linux/usbdevice_fs.h>`.
//!
//! Asynchronous transfers follow usbfs's model: the kernel copies a completed transfer's data,
//! status and packet lengths into the submitter's memory only when the transfer is *reaped*
//! (`USBDEVFS_REAPURB*`), never on its own. [`UrbRing`] owns that memory, at stable addresses,
//! until every submitted transfer is reaped (or leaks it if the kernel never gives one back),
//! so the safe API cannot let the kernel write freed memory.

mod urb;

use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lemnos_linux_sys::usbfs::{self as sys, UsbfsFile};

use crate::ioctl::{self, Ioctl, io};
use crate::{Error, Result};

pub use urb::{
    Completion, IsoPacket, MAX_ISO_PACKETS, TransferKind, UrbRing, UrbRingConfig, UrbStatus,
};

/// `struct usbdevfs_ioctl`.
#[repr(C)]
struct UsbIoctl {
    ifno: libc::c_int,
    ioctl_code: libc::c_int,
    data: *mut libc::c_void,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<UsbIoctl>() == 16);

ioctl::ioctls! {
    USBDEVFS_IOCTL = crate::ioctl::iowr::<UsbIoctl>(b'U', 18);
    USBDEVFS_DISCONNECT = io(b'U', 22);
    USBDEVFS_CONNECT = io(b'U', 23);
}

/// An `Error::Ioctl` named `name` for a failed Lemnos usbfs call.
fn usb_err(name: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| match source.raw_os_error() {
        Some(errno) => Error::Ioctl { name, errno },
        None => Error::Sys { call: name, source },
    }
}

fn millis(timeout: Duration) -> u32 {
    timeout.as_millis().min(u32::MAX as u128) as u32
}

/// The speed a device runs at (`USBDEVFS_GET_SPEED`, `enum usb_device_speed`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    Unknown,
    /// 1.5 Mbit/s.
    Low,
    /// 12 Mbit/s: 1 ms frames.
    Full,
    /// 480 Mbit/s: 125 µs microframes.
    High,
    /// Wireless USB.
    Wireless,
    /// 5 Gbit/s and faster: 125 µs bus intervals.
    Super,
    SuperPlus,
}

impl Speed {
    fn from_raw(v: i32) -> Speed {
        match v {
            1 => Speed::Low,
            2 => Speed::Full,
            3 => Speed::High,
            4 => Speed::Wireless,
            5 => Speed::Super,
            6 => Speed::SuperPlus,
            _ => Speed::Unknown,
        }
    }

    /// From sysfs `speed` (Mbit/s): `1.5`, `12`, `480`, `5000`, `10000`, `20000`.
    pub fn from_mbps(mbps: &str) -> Speed {
        match mbps.trim() {
            "1.5" => Speed::Low,
            "12" => Speed::Full,
            "480" => Speed::High,
            "5000" => Speed::Super,
            "10000" | "20000" => Speed::SuperPlus,
            _ => Speed::Unknown,
        }
    }

    /// The duration of one (micro)frame, the unit of isochronous scheduling.
    pub fn bus_interval(self) -> Duration {
        match self {
            Speed::Low | Speed::Full => Duration::from_millis(1),
            _ => Duration::from_micros(125),
        }
    }
}

/// A control transfer's setup packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlSetup {
    /// `bmRequestType`: direction (0x80 = device to host), type and recipient.
    pub request_type: u8,
    /// `bRequest`.
    pub request: u8,
    /// `wValue`.
    pub value: u16,
    /// `wIndex`.
    pub index: u16,
}

impl ControlSetup {
    /// Whether the data stage goes device to host.
    pub fn is_in(&self) -> bool {
        self.request_type & 0x80 != 0
    }
}

/// An open usbfs device node. The ioctls are Lemnos's (`lemnos_linux_sys::usbfs`); the
/// asynchronous transfers are [`UrbRing`]'s, on the same descriptor.
///
/// Every method takes `&self`: control transfers may run on one thread while another reaps
/// streaming transfers through the device's [`UrbRing`].
pub struct UsbDevice {
    file: UsbfsFile,
    path: PathBuf,
    /// A ring owns the asynchronous transfers of this descriptor (reaping returns any).
    ring_taken: AtomicBool,
}

impl std::fmt::Debug for UsbDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbDevice")
            .field("path", &self.path)
            .field("fd", &self.file.as_fd().as_raw_fd())
            .finish()
    }
}

impl UsbDevice {
    /// Opens `/dev/bus/usb/BBB/DDD` read-write.
    pub fn open(path: impl AsRef<Path>) -> Result<UsbDevice> {
        let path = path.as_ref();
        let file = UsbfsFile::open(path).map_err(|source| Error::Open {
            path: path.to_owned(),
            source,
        })?;
        Ok(UsbDevice {
            file,
            path: path.to_owned(),
            ring_taken: AtomicBool::new(false),
        })
    }

    /// The device node for bus `bus`, device `dev` (`/dev/bus/usb/003/002`).
    pub fn node_path(bus: u32, dev: u32) -> PathBuf {
        PathBuf::from(format!("/dev/bus/usb/{bus:03}/{dev:03}"))
    }

    /// The node this was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The device descriptor followed by every configuration's descriptors, as the kernel
    /// cached them at enumeration (what `read(2)` on the node returns; no bus traffic).
    /// Read with `pread` from the start, so every call returns them all.
    pub fn descriptors(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: `buf` is valid writable storage of `buf.len()` bytes; `pread` writes at
            // most that many.
            let n = unsafe {
                libc::pread(
                    self.file.as_fd().as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    out.len() as libc::off_t,
                )
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(Error::Sys {
                    call: "read(usbfs descriptors)",
                    source: e,
                });
            }
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
    }

    /// A control transfer on endpoint 0. For an IN request `data` receives up to its length
    /// (the returned count); for OUT it is sent. A request to an interface needs that
    /// interface claimed (the kernel claims it implicitly, failing with `EBUSY` while a kernel
    /// driver holds it).
    pub fn control(
        &self,
        setup: ControlSetup,
        data: &mut [u8],
        timeout: Duration,
    ) -> Result<usize> {
        let raw = sys::ControlSetup {
            request_type: setup.request_type,
            request: setup.request,
            value: setup.value,
            index: setup.index,
        };
        let r = if setup.is_in() {
            self.file.control_in(raw, data, millis(timeout))
        } else {
            self.file.control_out(raw, data, millis(timeout))
        };
        r.map_err(usb_err("USBDEVFS_CONTROL"))
    }

    /// A synchronous bulk (or interrupt) transfer on `endpoint` (bit 7 set for IN).
    pub fn bulk(&self, endpoint: u8, data: &mut [u8], timeout: Duration) -> Result<usize> {
        let r = if endpoint & 0x80 != 0 {
            self.file.bulk_in(endpoint, data, millis(timeout))
        } else {
            self.file.bulk_out(endpoint, data, millis(timeout))
        };
        r.map_err(usb_err("USBDEVFS_BULK"))
    }

    /// Selects configuration `value` (`bConfigurationValue`).
    pub fn set_configuration(&self, value: u32) -> Result<()> {
        let value = u8::try_from(value)
            .map_err(|_| Error::Invalid(format!("configuration value {value}")))?;
        self.file
            .set_configuration(value)
            .map_err(usb_err("USBDEVFS_SETCONFIGURATION"))
    }

    /// Claims interface `number` for this descriptor (`EBUSY` while a driver holds it).
    pub fn claim_interface(&self, number: u8) -> Result<()> {
        self.file
            .claim_interface(number, false)
            .map_err(usb_err("USBDEVFS_CLAIMINTERFACE"))
    }

    /// Releases interface `number`.
    pub fn release_interface(&self, number: u8) -> Result<()> {
        self.file
            .release_interface(number, false)
            .map_err(usb_err("USBDEVFS_RELEASEINTERFACE"))
    }

    /// Detaches whatever kernel driver holds interface `number` and claims it, atomically
    /// (`USBDEVFS_DISCONNECT_CLAIM`). Only that interface: the device's other interfaces keep
    /// their drivers.
    pub fn detach_and_claim(&self, number: u8) -> Result<()> {
        self.file
            .claim_interface(number, true)
            .map_err(usb_err("USBDEVFS_DISCONNECT_CLAIM"))
    }

    /// The kernel driver bound to interface `number`, if any (`usbfs` for one claimed through
    /// usbfs).
    pub fn driver(&self, number: u8) -> Result<Option<String>> {
        self.file
            .driver(number)
            .map_err(usb_err("USBDEVFS_GETDRIVER"))
    }

    /// Detaches the kernel driver from interface `number` (`USBDEVFS_DISCONNECT`).
    pub fn detach_kernel_driver(&self, number: u8) -> Result<()> {
        self.interface_ioctl(number, USBDEVFS_DISCONNECT)
    }

    /// Lets the kernel bind a driver to interface `number` again (`USBDEVFS_CONNECT`); the
    /// interface must not be claimed.
    pub fn attach_kernel_driver(&self, number: u8) -> Result<()> {
        self.interface_ioctl(number, USBDEVFS_CONNECT)
    }

    fn interface_ioctl(&self, number: u8, code: Ioctl) -> Result<()> {
        let mut arg = UsbIoctl {
            ifno: number.into(),
            ioctl_code: code.nr as libc::c_int,
            data: std::ptr::null_mut(),
        };
        // SAFETY: `arg` is a `usbdevfs_ioctl` with no data for these two codes.
        unsafe { ioctl::ioctl(self.file.as_fd(), USBDEVFS_IOCTL, &mut arg) }
            .map(|_| ())
            .map_err(|e| match e {
                Error::Ioctl { errno, .. } => Error::Ioctl {
                    name: code.name,
                    errno,
                },
                e => e,
            })
    }

    /// Selects alternate setting `alt` of interface `number` (claimed).
    pub fn set_interface(&self, number: u8, alt: u8) -> Result<()> {
        self.file
            .set_interface(number, alt)
            .map_err(usb_err("USBDEVFS_SETINTERFACE"))
    }

    /// Clears a halt (stall) on `endpoint`.
    pub fn clear_halt(&self, endpoint: u8) -> Result<()> {
        self.file
            .clear_halt(endpoint)
            .map_err(usb_err("USBDEVFS_CLEAR_HALT"))
    }

    /// Resets the device (it re-enumerates if its descriptors changed).
    pub fn reset(&self) -> Result<()> {
        self.file.reset().map_err(usb_err("USBDEVFS_RESET"))
    }

    /// The device's speed.
    pub fn speed(&self) -> Result<Speed> {
        let v = self.file.speed().map_err(usb_err("USBDEVFS_GET_SPEED"))?;
        Ok(Speed::from_raw(v))
    }

    /// usbfs capability bits (`USBDEVFS_CAP_*`).
    pub fn capabilities(&self) -> Result<u32> {
        self.file
            .capabilities()
            .map_err(usb_err("USBDEVFS_GET_CAPABILITIES"))
    }

    /// Waits until a submitted transfer completed (`POLLOUT`) or the device went away.
    pub fn wait(&self, timeout: Option<Duration>) -> Result<crate::Ready> {
        ioctl::poll_fd(
            self.file.as_fd(),
            libc::POLLOUT,
            timeout.map(crate::ioctl::ceil_ms),
        )
    }

    pub(crate) fn take_ring(&self) -> Result<()> {
        if self.ring_taken.swap(true, Ordering::AcqRel) {
            return Err(Error::Invalid(
                "this usbfs descriptor already has a transfer ring".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn give_back_ring(&self) {
        self.ring_taken.store(false, Ordering::Release);
    }
}

impl AsFd for UsbDevice {
    /// The descriptor reports `POLLOUT` when a submitted transfer completed, `POLLHUP` when
    /// the device was disconnected.
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

/// Lists the usbfs nodes under `/dev/bus/usb`, sorted.
pub fn list_nodes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for bus in std::fs::read_dir("/dev/bus/usb")
        .into_iter()
        .flatten()
        .flatten()
    {
        for dev in std::fs::read_dir(bus.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            out.push(dev.path());
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        // From <linux/usbdevice_fs.h> on x86_64/aarch64.
        assert_eq!(USBDEVFS_IOCTL.nr, 0xc010_5512);
        assert_eq!(USBDEVFS_DISCONNECT.nr, 0x5516);
        assert_eq!(USBDEVFS_CONNECT.nr, 0x5517);
    }

    #[test]
    fn speeds_and_paths() {
        assert_eq!(Speed::from_mbps("480\n"), Speed::High);
        assert_eq!(Speed::High.bus_interval(), Duration::from_micros(125));
        assert_eq!(Speed::Full.bus_interval(), Duration::from_millis(1));
        assert_eq!(
            UsbDevice::node_path(3, 2),
            PathBuf::from("/dev/bus/usb/003/002")
        );
    }

    #[test]
    fn opens_and_reads_descriptors_where_a_device_exists() {
        let Some(node) = list_nodes().into_iter().next() else {
            return;
        };
        // Usually not writable without privileges: skip then.
        let Ok(dev) = UsbDevice::open(&node) else {
            return;
        };
        let d = dev.descriptors().unwrap();
        assert!(d.len() >= 18 && d[1] == 1, "{node:?}: {d:?}");
    }
}
