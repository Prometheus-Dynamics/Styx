//! Kernel uevents (`NETLINK_KOBJECT_UEVENT`): devices appearing, going away, binding to drivers.
//! What udev listens to, without udev. Unprivileged processes may listen.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::time::Duration;

use crate::{Error, Result};

const NETLINK_KOBJECT_UEVENT: libc::c_int = 15;
/// The kernel's multicast group (udev re-broadcasts on group 2).
const KERNEL_GROUP: u32 = 1;

/// One uevent: `ACTION@DEVPATH` and its `KEY=VALUE` variables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uevent {
    /// `add`, `remove`, `bind`, `unbind`, `change`, `move`, `online`, `offline`.
    pub action: String,
    /// The sysfs path below `/sys` (`/devices/.../usb3/3-1`).
    pub devpath: String,
    /// The variables (`SUBSYSTEM`, `DEVTYPE`, `DEVNAME`, `BUSNUM`, `DEVNUM`, `PRODUCT`, ...).
    pub vars: Vec<(String, String)>,
}

impl Uevent {
    /// Parses a kernel uevent message (`action@devpath\0KEY=VALUE\0...`).
    pub fn parse(msg: &[u8]) -> Option<Uevent> {
        let mut parts = msg.split(|&b| b == 0).filter(|p| !p.is_empty());
        let head = std::str::from_utf8(parts.next()?).ok()?;
        let (action, devpath) = head.split_once('@')?;
        let vars = parts
            .filter_map(|p| {
                let s = std::str::from_utf8(p).ok()?;
                let (k, v) = s.split_once('=')?;
                Some((k.to_owned(), v.to_owned()))
            })
            .collect();
        Some(Uevent {
            action: action.to_owned(),
            devpath: devpath.to_owned(),
            vars,
        })
    }

    /// A variable's value.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// A socket receiving the kernel's uevents. Non-blocking: [`UeventSocket::recv`] returns
/// `Ok(None)` when nothing is pending; the descriptor is readable when one is.
#[derive(Debug)]
pub struct UeventSocket {
    fd: OwnedFd,
}

impl UeventSocket {
    /// Opens and binds the socket to the kernel's uevent group.
    pub fn open() -> Result<UeventSocket> {
        // SAFETY: plain socket creation; the result is checked below.
        let raw = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                NETLINK_KOBJECT_UEVENT,
            )
        };
        if raw < 0 {
            return Err(Error::sys("socket(NETLINK_KOBJECT_UEVENT)"));
        }
        // SAFETY: `socket` returned a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: all-zero is a valid `sockaddr_nl`.
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_groups = KERNEL_GROUP;
        // SAFETY: `addr` is a valid `sockaddr_nl` of the given length for the call.
        let r = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_nl).cast(),
                size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if r < 0 {
            return Err(Error::sys("bind(NETLINK_KOBJECT_UEVENT)"));
        }
        Ok(UeventSocket { fd })
    }

    /// The next uevent from the kernel, if one is pending (messages from other senders are
    /// skipped).
    pub fn recv(&self) -> Result<Option<Uevent>> {
        let mut buf = vec![0u8; 8192];
        loop {
            // SAFETY: all-zero is a valid `sockaddr_nl`.
            let mut from: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
            let mut from_len = size_of::<libc::sockaddr_nl>() as libc::socklen_t;
            // SAFETY: `buf` and `from` are valid writable storage of the given lengths.
            let n = unsafe {
                libc::recvfrom(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    0,
                    (&mut from as *mut libc::sockaddr_nl).cast(),
                    &mut from_len,
                )
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EAGAIN) => return Ok(None),
                    // ENOBUFS (an overrun: events were lost, the caller rescans) and the rest.
                    _ => {
                        return Err(Error::Sys {
                            call: "recv(uevent)",
                            source: e,
                        });
                    }
                }
            }
            // Only the kernel (port 0) sends real uevents on this group.
            if from.nl_pid != 0 {
                continue;
            }
            if let Some(ev) = Uevent::parse(&buf[..n as usize]) {
                return Ok(Some(ev));
            }
        }
    }

    /// Waits until a uevent is pending or `timeout` passes.
    pub fn wait(&self, timeout: Option<Duration>) -> Result<crate::Ready> {
        crate::ioctl::poll_fd(
            self.fd.as_fd(),
            libc::POLLIN,
            timeout.map(crate::ioctl::ceil_ms),
        )
    }
}

impl AsFd for UeventSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_usb_add() {
        let msg = b"add@/devices/platform/xhci-hcd.1/usb3/3-1\0ACTION=add\0DEVPATH=/devices/platform/xhci-hcd.1/usb3/3-1\0SUBSYSTEM=usb\0DEVNAME=bus/usb/003/002\0DEVTYPE=usb_device\0PRODUCT=46d/825/12\0BUSNUM=003\0DEVNUM=002\0SEQNUM=1234\0";
        let ev = Uevent::parse(msg).unwrap();
        assert_eq!(ev.action, "add");
        assert_eq!(ev.devpath, "/devices/platform/xhci-hcd.1/usb3/3-1");
        assert_eq!(ev.get("DEVTYPE"), Some("usb_device"));
        assert_eq!(ev.get("BUSNUM"), Some("003"));
        assert_eq!(ev.get("MISSING"), None);
        assert!(Uevent::parse(b"libudev\0junk").is_none());
    }

    #[test]
    fn opens_where_netlink_is_allowed() {
        let Ok(sock) = UeventSocket::open() else {
            return;
        };
        assert!(sock.recv().is_ok());
    }
}
