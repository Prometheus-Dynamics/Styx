//! V4L2 video nodes: capabilities, formats, sizes and frame intervals, buffers (mmap and
//! dma-buf), streaming, controls.
//!
//! ```no_run
//! use styx_kernel::v4l2::{BufType, VideoDevice};
//!
//! let dev = VideoDevice::open("/dev/video0")?;
//! println!("{} ({})", dev.capabilities().card, dev.capabilities().driver);
//! for fmt in dev.formats(BufType::VideoCapture)? {
//!     println!("{} {}", fmt.fourcc, fmt.description);
//! }
//! # Ok::<(), styx_kernel::Error>(())
//! ```

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::flags::flags;
use crate::ioctl::{self, cstr_field};
use crate::{Ready, Result};

mod buffer;
mod capture;
pub mod cid;
mod control;
mod format;
pub(crate) mod raw;

pub use buffer::{
    BufferCapabilities, BufferFlags, BufferInfo, DequeuedBuffer, Memory, PlaneInfo, QueueBuffer,
    QueuePlane, RequestedBuffers,
};
pub use capture::DmaBufAccess;
pub use control::{
    ControlFlags, ControlInfo, ControlType, ControlValue, ControlWhich, Controls, MenuItem,
    MenuValue,
};
pub use format::{
    Format, FormatDesc, FormatFlags, FrameIntervals, FrameSize, FrameSizes, MetaFormat, PixFormat,
    PixFormatMplane, PlaneFormat, SelectionTarget, StepwiseSize, StreamParams,
};

/// The kind of buffer queue an operation applies to (`enum v4l2_buf_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum BufType {
    /// Single-planar video capture.
    VideoCapture = 1,
    /// Single-planar video output.
    VideoOutput = 2,
    /// Video overlay.
    VideoOverlay = 3,
    /// Raw VBI capture.
    VbiCapture = 4,
    /// Raw VBI output.
    VbiOutput = 5,
    /// Sliced VBI capture.
    SlicedVbiCapture = 6,
    /// Sliced VBI output.
    SlicedVbiOutput = 7,
    /// Video output overlay.
    VideoOutputOverlay = 8,
    /// Multi-planar video capture.
    VideoCaptureMplane = 9,
    /// Multi-planar video output.
    VideoOutputMplane = 10,
    /// Software-defined radio capture.
    SdrCapture = 11,
    /// Software-defined radio output.
    SdrOutput = 12,
    /// Metadata capture (statistics, embedded data).
    MetaCapture = 13,
    /// Metadata output (ISP parameters).
    MetaOutput = 14,
}

impl BufType {
    /// Converts a raw `v4l2_buf_type`.
    pub fn from_raw(v: u32) -> Option<Self> {
        use BufType::*;
        Some(match v {
            1 => VideoCapture,
            2 => VideoOutput,
            3 => VideoOverlay,
            4 => VbiCapture,
            5 => VbiOutput,
            6 => SlicedVbiCapture,
            7 => SlicedVbiOutput,
            8 => VideoOutputOverlay,
            9 => VideoCaptureMplane,
            10 => VideoOutputMplane,
            11 => SdrCapture,
            12 => SdrOutput,
            13 => MetaCapture,
            14 => MetaOutput,
            _ => return None,
        })
    }

    /// The raw value.
    pub fn to_raw(self) -> u32 {
        self as u32
    }

    /// True for the multi-planar video queue types.
    pub fn is_multiplanar(self) -> bool {
        matches!(
            self,
            BufType::VideoCaptureMplane | BufType::VideoOutputMplane
        )
    }

    /// True for metadata queues.
    pub fn is_meta(self) -> bool {
        matches!(self, BufType::MetaCapture | BufType::MetaOutput)
    }

    /// True for queues userspace fills (output, as seen from the application).
    pub fn is_output(self) -> bool {
        matches!(
            self,
            BufType::VideoOutput
                | BufType::VbiOutput
                | BufType::SlicedVbiOutput
                | BufType::VideoOutputOverlay
                | BufType::VideoOutputMplane
                | BufType::SdrOutput
                | BufType::MetaOutput
        )
    }
}

flags! {
    /// Device capability flags (`V4L2_CAP_*`).
    pub struct CapabilityFlags: u32 {
        const VIDEO_CAPTURE = 0x0000_0001;
        const VIDEO_OUTPUT = 0x0000_0002;
        const VIDEO_OVERLAY = 0x0000_0004;
        const VBI_CAPTURE = 0x0000_0010;
        const VBI_OUTPUT = 0x0000_0020;
        const SLICED_VBI_CAPTURE = 0x0000_0040;
        const SLICED_VBI_OUTPUT = 0x0000_0080;
        const RDS_CAPTURE = 0x0000_0100;
        const VIDEO_OUTPUT_OVERLAY = 0x0000_0200;
        const HW_FREQ_SEEK = 0x0000_0400;
        const RDS_OUTPUT = 0x0000_0800;
        const VIDEO_CAPTURE_MPLANE = 0x0000_1000;
        const VIDEO_OUTPUT_MPLANE = 0x0000_2000;
        const VIDEO_M2M_MPLANE = 0x0000_4000;
        const VIDEO_M2M = 0x0000_8000;
        const TUNER = 0x0001_0000;
        const AUDIO = 0x0002_0000;
        const RADIO = 0x0004_0000;
        const MODULATOR = 0x0008_0000;
        const SDR_CAPTURE = 0x0010_0000;
        const EXT_PIX_FORMAT = 0x0020_0000;
        const SDR_OUTPUT = 0x0040_0000;
        const META_CAPTURE = 0x0080_0000;
        const READWRITE = 0x0100_0000;
        const EDID = 0x0200_0000;
        const STREAMING = 0x0400_0000;
        const META_OUTPUT = 0x0800_0000;
        const TOUCH = 0x1000_0000;
        const IO_MC = 0x2000_0000;
        const DEVICE_CAPS = 0x8000_0000;
    }
}

/// The kernel version a driver reports (`KERNEL_VERSION(major, minor, patch)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KernelVersion {
    /// Major.
    pub major: u8,
    /// Minor.
    pub minor: u8,
    /// Patch level.
    pub patch: u8,
}

impl KernelVersion {
    /// Decodes a `KERNEL_VERSION()` value.
    pub fn from_raw(v: u32) -> Self {
        Self {
            major: (v >> 16) as u8,
            minor: (v >> 8) as u8,
            patch: v as u8,
        }
    }
}

impl std::fmt::Display for KernelVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// What a video node is and can do (`VIDIOC_QUERYCAP`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Driver name, e.g. `uvcvideo`, `rp1-cfe`.
    pub driver: String,
    /// Device name, e.g. `UVC Camera (046d:0825)`.
    pub card: String,
    /// Bus location, e.g. `usb-xhci-hcd.1-1`, `platform:1f00128000.csi`.
    pub bus_info: String,
    /// Driver (kernel) version.
    pub version: KernelVersion,
    /// Capabilities of the whole physical device.
    pub capabilities: CapabilityFlags,
    /// Capabilities of this node (equal to `capabilities` for drivers that do not report them
    /// separately).
    pub device_caps: CapabilityFlags,
}

impl Capabilities {
    fn from_raw(raw: &raw::v4l2_capability) -> Self {
        let capabilities = CapabilityFlags(raw.capabilities);
        let device_caps = if capabilities.contains(CapabilityFlags::DEVICE_CAPS) {
            CapabilityFlags(raw.device_caps)
        } else {
            capabilities
        };
        Self {
            driver: cstr_field(&raw.driver),
            card: cstr_field(&raw.card),
            bus_info: cstr_field(&raw.bus_info),
            version: KernelVersion::from_raw(raw.version),
            capabilities,
            device_caps,
        }
    }

    /// The buffer queue types this node supports, from its device capabilities.
    pub fn buffer_types(&self) -> Vec<BufType> {
        let caps = self.device_caps;
        let mut out = Vec::new();
        let table = [
            (CapabilityFlags::VIDEO_CAPTURE, BufType::VideoCapture),
            (
                CapabilityFlags::VIDEO_CAPTURE_MPLANE,
                BufType::VideoCaptureMplane,
            ),
            (CapabilityFlags::VIDEO_OUTPUT, BufType::VideoOutput),
            (
                CapabilityFlags::VIDEO_OUTPUT_MPLANE,
                BufType::VideoOutputMplane,
            ),
            (CapabilityFlags::META_CAPTURE, BufType::MetaCapture),
            (CapabilityFlags::META_OUTPUT, BufType::MetaOutput),
        ];
        for (flag, ty) in table {
            if caps.contains(flag) {
                out.push(ty);
            }
        }
        if caps.contains(CapabilityFlags::VIDEO_M2M) {
            out.extend([BufType::VideoCapture, BufType::VideoOutput]);
        }
        if caps.contains(CapabilityFlags::VIDEO_M2M_MPLANE) {
            out.extend([BufType::VideoCaptureMplane, BufType::VideoOutputMplane]);
        }
        out.dedup();
        out
    }

    /// True when the node's inputs/outputs are configured through the media controller
    /// (`V4L2_CAP_IO_MC`), as on `rp1-cfe` and `pispbe`.
    pub fn is_media_controller_centric(&self) -> bool {
        self.device_caps.contains(CapabilityFlags::IO_MC)
    }
}

/// An open V4L2 video node (`/dev/videoN`).
///
/// The descriptor is non-blocking: [`VideoDevice::dequeue`] returns `Ok(None)` when no buffer is
/// ready. Wait for readiness with [`VideoDevice::wait`] or by registering [`AsFd::as_fd`] with an
/// async reactor (readable for capture, writable for output, priority for events).
#[derive(Debug)]
pub struct VideoDevice {
    fd: OwnedFd,
    path: PathBuf,
    caps: Capabilities,
}

impl VideoDevice {
    /// Opens a video node (read-write, non-blocking, close-on-exec) and queries its
    /// capabilities.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let fd = ioctl::open_device(path, true)?;
        let caps = query_capabilities(fd.as_fd())?;
        Ok(Self {
            fd,
            path: path.to_owned(),
            caps,
        })
    }

    /// Opens a video node read-only (non-blocking, close-on-exec): enough for querying,
    /// enumerating and reading controls; most drivers refuse changes on such a handle.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let fd = ioctl::open_path(path, libc::O_RDONLY, true)?;
        let caps = query_capabilities(fd.as_fd())?;
        Ok(Self {
            fd,
            path: path.to_owned(),
            caps,
        })
    }

    /// The path the node was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The capabilities queried when the node was opened.
    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// Queries the capabilities again.
    pub fn query_capabilities(&self) -> Result<Capabilities> {
        query_capabilities(self.fd.as_fd())
    }

    /// Switches the descriptor between non-blocking (the default) and blocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> Result<()> {
        ioctl::set_nonblocking(self.fd.as_fd(), nonblocking)
    }

    /// Waits until a buffer can be dequeued (readable/writable), an event is pending
    /// (priority), or the timeout passes. `None` waits forever.
    pub fn wait(&self, timeout: Option<Duration>) -> Result<Ready> {
        ioctl::poll_fd(
            self.fd.as_fd(),
            libc::POLLIN | libc::POLLOUT | libc::POLLPRI,
            timeout,
        )
    }
}

fn query_capabilities(fd: BorrowedFd<'_>) -> Result<Capabilities> {
    let mut raw: raw::v4l2_capability = raw::zeroed();
    // SAFETY: VIDIOC_QUERYCAP takes a `v4l2_capability`.
    unsafe { ioctl::ioctl(fd, raw::VIDIOC_QUERYCAP, &mut raw)? };
    Ok(Capabilities::from_raw(&raw))
}

impl AsFd for VideoDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl AsRawFd for VideoDevice {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl Controls for VideoDevice {}
impl crate::event::Events for VideoDevice {}

/// Lists `/dev/video*` nodes, sorted by number.
pub fn list_video_nodes() -> Vec<PathBuf> {
    crate::ioctl::list_dev_nodes("video")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buf_type_round_trip() {
        for v in 1..=14 {
            assert_eq!(BufType::from_raw(v).unwrap().to_raw(), v);
        }
        assert!(BufType::from_raw(0).is_none());
        assert!(BufType::VideoCaptureMplane.is_multiplanar());
        assert!(BufType::MetaOutput.is_output());
    }

    #[test]
    fn capabilities_prefer_device_caps() {
        let mut raw: raw::v4l2_capability = raw::zeroed();
        raw.driver[..8].copy_from_slice(b"uvcvideo");
        raw.capabilities = 0x8400_0001 | 0x0080_0000;
        raw.device_caps = 0x0400_0001;
        raw.version = (6 << 16) | (12 << 8) | 47;
        let caps = Capabilities::from_raw(&raw);
        assert_eq!(caps.driver, "uvcvideo");
        assert_eq!(caps.version.to_string(), "6.12.47");
        assert_eq!(caps.buffer_types(), vec![BufType::VideoCapture]);
        assert!(!caps.device_caps.contains(CapabilityFlags::META_CAPTURE));
    }
}
