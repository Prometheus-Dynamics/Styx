//! The media controller: device info, topology (entities, pads, links, interfaces), link setup
//! and requests.
//!
//! ```no_run
//! use styx_kernel::media::MediaDevice;
//!
//! let media = MediaDevice::open("/dev/media0")?;
//! let topo = media.topology()?;
//! for link in topo.data_links() {
//!     let src = topo.entity(link.source.entity).unwrap();
//!     let sink = topo.entity(link.sink.entity).unwrap();
//!     println!("{}:{} -> {}:{}", src.name, link.source.index, sink.name, link.sink.index);
//! }
//! # Ok::<(), styx_kernel::Error>(())
//! ```

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ioctl::{self, cstr_field};
use crate::v4l2::raw::zeroed;
use crate::{Error, Ready, Result};

mod raw;
mod topology;

pub use topology::{
    DataLink, DevNode, Entity, EntityFlags, EntityFunction, Interface, InterfaceType, Link,
    LinkFlags, LinkType, Pad, PadFlags, PadRef, Topology,
};

/// Build a topology from what `MEDIA_IOC_G_TOPOLOGY` would fill in, from `bytes` (four
/// counts, then the arrays), and resolve every link. For fuzzing.
#[doc(hidden)]
pub fn fuzz_topology(bytes: &[u8]) {
    fn take<T: Copy>(bytes: &mut &[u8], count: u8) -> Vec<T> {
        (0..count)
            .map(|_| {
                let n = bytes.len().min(size_of::<T>());
                let value = crate::v4l2::raw::from_bytes::<T>(&bytes[..n]);
                *bytes = &bytes[n..];
                value
            })
            .collect()
    }
    let Some((counts, mut rest)) = bytes.split_first_chunk::<4>() else {
        return;
    };
    let entities = take::<raw::media_v2_entity>(&mut rest, counts[0] % 32);
    let interfaces = take::<raw::media_v2_interface>(&mut rest, counts[1] % 32);
    let pads = take::<raw::media_v2_pad>(&mut rest, counts[2] % 64);
    let links = take::<raw::media_v2_link>(&mut rest, counts[3] % 64);
    let topology = Topology::from_raw(7, &entities, &interfaces, &pads, &links);
    let _ = topology.data_links();
    for e in &topology.entities {
        let _ = topology.entity_by_name(&e.name);
        let _ = (topology.pads_of(e.id), topology.links_from(e.id));
        let _ = (topology.links_to(e.id), topology.interface_of(e.id));
        let _ = (e.function.name(), e.flags);
    }
    for i in &topology.interfaces {
        let _ = (topology.entity_of_interface(i.id), i.intf_type.name());
    }
    for l in &topology.links {
        let _ = l.flags.link_type();
    }
}

/// Information about a media device (`MEDIA_IOC_DEVICE_INFO`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Driver name, e.g. `rp1-cfe`, `uvcvideo`.
    pub driver: String,
    /// Model, e.g. `rp1-cfe`, `UVC Camera (046d:0825)`.
    pub model: String,
    /// Serial number (may be empty).
    pub serial: String,
    /// Bus location.
    pub bus_info: String,
    /// Media API version (`KERNEL_VERSION`-encoded).
    pub media_version: u32,
    /// Hardware revision.
    pub hw_revision: u32,
    /// Driver version (`KERNEL_VERSION`-encoded).
    pub driver_version: u32,
}

/// An open media controller device (`/dev/mediaN`).
#[derive(Debug)]
pub struct MediaDevice {
    fd: OwnedFd,
    path: PathBuf,
}

impl MediaDevice {
    /// Opens a media device read-write, close-on-exec.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        Ok(Self {
            fd: ioctl::open_device(path, false)?,
            path: path.to_owned(),
        })
    }

    /// Opens a media device read-only: device info and topology work, link setup and request
    /// allocation do not.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        Ok(Self {
            fd: ioctl::open_path(path, libc::O_RDONLY, false)?,
            path: path.to_owned(),
        })
    }

    /// The path it was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queries the device information (`MEDIA_IOC_DEVICE_INFO`).
    pub fn device_info(&self) -> Result<DeviceInfo> {
        let mut raw: raw::media_device_info = zeroed();
        // SAFETY: MEDIA_IOC_DEVICE_INFO takes a `media_device_info`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::MEDIA_IOC_DEVICE_INFO, &mut raw)? };
        Ok(DeviceInfo {
            driver: cstr_field(&raw.driver),
            model: cstr_field(&raw.model),
            serial: cstr_field(&raw.serial),
            bus_info: cstr_field(&raw.bus_info),
            media_version: raw.media_version,
            hw_revision: raw.hw_revision,
            driver_version: raw.driver_version,
        })
    }

    /// Reads the whole graph (`MEDIA_IOC_G_TOPOLOGY`), retrying if it changes while being read.
    pub fn topology(&self) -> Result<Topology> {
        for _ in 0..8 {
            let mut counts = raw::media_v2_topology::default();
            // SAFETY: MEDIA_IOC_G_TOPOLOGY takes a `media_v2_topology`; with null array pointers
            // the kernel only fills in the counts.
            unsafe { ioctl::ioctl(self.as_fd(), raw::MEDIA_IOC_G_TOPOLOGY, &mut counts)? };
            let mut entities = vec![zeroed::<raw::media_v2_entity>(); counts.num_entities as usize];
            let mut interfaces =
                vec![zeroed::<raw::media_v2_interface>(); counts.num_interfaces as usize];
            let mut pads = vec![zeroed::<raw::media_v2_pad>(); counts.num_pads as usize];
            let mut links = vec![zeroed::<raw::media_v2_link>(); counts.num_links as usize];
            let mut topo = raw::media_v2_topology {
                num_entities: counts.num_entities,
                ptr_entities: entities.as_mut_ptr() as u64,
                num_interfaces: counts.num_interfaces,
                ptr_interfaces: interfaces.as_mut_ptr() as u64,
                num_pads: counts.num_pads,
                ptr_pads: pads.as_mut_ptr() as u64,
                num_links: counts.num_links,
                ptr_links: links.as_mut_ptr() as u64,
                ..Default::default()
            };
            // SAFETY: each pointer refers to a vector with room for the matching count; the
            // vectors outlive the call.
            match unsafe { ioctl::ioctl(self.as_fd(), raw::MEDIA_IOC_G_TOPOLOGY, &mut topo) } {
                Ok(_) => {}
                // The graph grew between the two calls.
                Err(e) if e.errno() == Some(libc::ENOSPC) => continue,
                Err(e) => return Err(e),
            }
            if { topo.topology_version } != { counts.topology_version } {
                continue;
            }
            entities.truncate(topo.num_entities as usize);
            interfaces.truncate(topo.num_interfaces as usize);
            pads.truncate(topo.num_pads as usize);
            links.truncate(topo.num_links as usize);
            return Ok(Topology::from_raw(
                topo.topology_version,
                &entities,
                &interfaces,
                &pads,
                &links,
            ));
        }
        Err(Error::Invalid(
            "media graph kept changing while being read".into(),
        ))
    }

    /// Enables or disables a data link between two pads (`MEDIA_IOC_SETUP_LINK`). `flags` is
    /// usually [`LinkFlags::ENABLED`] or empty; immutable links cannot be changed.
    pub fn setup_link(&self, source: PadRef, sink: PadRef, flags: LinkFlags) -> Result<()> {
        let pad = |p: PadRef| raw::media_pad_desc {
            entity: p.entity,
            index: p.index as u16,
            ..Default::default()
        };
        let mut desc = raw::media_link_desc {
            source: pad(source),
            sink: pad(sink),
            flags: flags.0,
            ..Default::default()
        };
        // SAFETY: MEDIA_IOC_SETUP_LINK takes a `media_link_desc`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::MEDIA_IOC_SETUP_LINK, &mut desc)? };
        Ok(())
    }

    /// The process holding a record lock on the device node (`fcntl(F_GETLK)` for a write
    /// lock over the whole file), as libcamera takes one (`lockf`) while one of the device's
    /// cameras is acquired. Takes no lock itself; this process's own locks are not reported.
    pub fn lock_holder(&self) -> Result<Option<i32>> {
        // SAFETY: `flock` is plain data; all zeros is a valid value.
        let mut fl: libc::flock = unsafe { std::mem::zeroed() };
        fl.l_type = libc::F_WRLCK as libc::c_short;
        fl.l_whence = libc::SEEK_SET as libc::c_short;
        // SAFETY: F_GETLK takes a pointer to a `flock` that lives for the call.
        if unsafe { libc::fcntl(self.as_raw_fd(), libc::F_GETLK, &mut fl) } < 0 {
            return Err(Error::sys("fcntl(F_GETLK)"));
        }
        Ok((i32::from(fl.l_type) != libc::F_UNLCK).then_some(fl.l_pid))
    }

    /// Allocates a request (`MEDIA_IOC_REQUEST_ALLOC`) for the request API.
    pub fn alloc_request(&self) -> Result<Request> {
        let mut fd: libc::c_int = -1;
        // SAFETY: MEDIA_IOC_REQUEST_ALLOC writes an int file descriptor.
        unsafe { ioctl::ioctl(self.as_fd(), raw::MEDIA_IOC_REQUEST_ALLOC, &mut fd)? };
        if fd < 0 {
            return Err(Error::Invalid(
                "MEDIA_IOC_REQUEST_ALLOC returned no fd".into(),
            ));
        }
        // SAFETY: the kernel returned a new request descriptor that we now own.
        Ok(Request {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }
}

impl AsFd for MediaDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl AsRawFd for MediaDevice {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// A media request: a set of buffers and control values applied together.
///
/// Fill it by queueing buffers with [`crate::v4l2::QueueBuffer::request`] and setting controls
/// with [`crate::v4l2::ControlWhich::Request`], then [`Request::queue`] it. Completion makes the
/// descriptor ready for priority (`POLLPRI`).
#[derive(Debug)]
pub struct Request {
    fd: OwnedFd,
}

impl Request {
    /// Queues the request (`MEDIA_REQUEST_IOC_QUEUE`).
    pub fn queue(&self) -> Result<()> {
        // SAFETY: MEDIA_REQUEST_IOC_QUEUE takes no argument.
        unsafe {
            ioctl::ioctl_raw(
                self.as_fd(),
                raw::MEDIA_REQUEST_IOC_QUEUE,
                std::ptr::null_mut(),
            )?
        };
        Ok(())
    }

    /// Empties a completed (or never queued) request for reuse (`MEDIA_REQUEST_IOC_REINIT`).
    pub fn reinit(&self) -> Result<()> {
        // SAFETY: MEDIA_REQUEST_IOC_REINIT takes no argument.
        unsafe {
            ioctl::ioctl_raw(
                self.as_fd(),
                raw::MEDIA_REQUEST_IOC_REINIT,
                std::ptr::null_mut(),
            )?
        };
        Ok(())
    }

    /// Waits until the request completes, or the timeout passes (`false`).
    pub fn wait(&self, timeout: Option<Duration>) -> Result<bool> {
        let ready: Ready = ioctl::poll_fd(self.as_fd(), libc::POLLPRI, timeout)?;
        Ok(ready.priority)
    }
}

impl AsFd for Request {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl AsRawFd for Request {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Lists `/dev/media*` nodes, sorted by number.
pub fn list_media_devices() -> Vec<PathBuf> {
    ioctl::list_dev_nodes("media")
}
