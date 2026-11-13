//! A frame, held until the C caller releases it.
//!
//! A frame is one of two things: a lease from the in-process capture, or a frame the frame socket
//! delivered (a `styx-socket` frame, its buffer mapped read-only in this process). Either way the
//! planes are read where the producer wrote them: nothing is copied, and the caller's pointers are
//! valid until the frame is released.

use std::os::fd::AsRawFd;

use styx_controls::{FrameMetadata, WIRE_BYTES};
use styx_frame::{BufferMemory, CpuRead, FrameLease, MemoryKind};

use crate::status::{E_PROTOCOL, E_UNSUPPORTED, Fail, Res};

/// Memory kinds as the header names them.
pub const MEMORY_HOST: u32 = 0;
pub const MEMORY_MEMFD: u32 = 1;
pub const MEMORY_DMABUF: u32 = 2;
pub const MEMORY_GPU: u32 = 3;

/// One frame's identity, format and metadata, as the header's `styx_frame_info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct FrameInfo {
    pub id: u64,
    pub timestamp_ns: u64,
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub planes: u32,
    pub fps_milli: u32,
    pub exposure_us: u32,
    pub analogue_gain_milli: u32,
    pub memory_kind: u32,
    /// The producer's achieved frame rate, from the metadata (0 when unknown or a version 1 record).
    pub achieved_fps_milli: u32,
}

/// One plane's bytes, as the header's `styx_plane`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct PlaneOut {
    pub data: *const u8,
    pub len: usize,
    pub stride: usize,
}

/// A frame's buffer as a descriptor, for a zero-copy import elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaView {
    pub fd: i32,
    pub offset: u64,
    pub len: u64,
    pub kind: u32,
}

#[derive(Debug)]
// The socket frame is large (its user bytes are inline). The C handle already boxes each frame, so
// the body stays inline: boxing it would add an allocation per frame.
#[allow(clippy::large_enum_variant)]
enum Body {
    Local(FrameLease<FrameMetadata>),
    Remote(styx_socket::Frame),
}

/// A frame the C caller holds.
#[derive(Debug)]
pub struct Frame {
    body: Body,
    meta: FrameMetadata,
    fps_milli: u32,
}

impl Frame {
    pub(crate) fn local(lease: FrameLease<FrameMetadata>) -> Self {
        let meta = *lease.meta();
        Self {
            body: Body::Local(lease),
            meta,
            fps_milli: 0,
        }
    }

    /// A frame from the socket, whose metadata is the producer's wire record.
    pub(crate) fn remote(frame: styx_socket::Frame) -> Res<Self> {
        let meta = FrameMetadata::from_bytes(frame.user()).ok_or_else(|| {
            Fail::new(
                E_PROTOCOL,
                "the frame's metadata is not wire version 1 (or is missing)",
            )
        })?;
        Ok(Self {
            body: Body::Remote(frame),
            meta,
            fps_milli: 0,
        })
    }

    pub(crate) fn set_fps(&mut self, fps_milli: u32) {
        self.fps_milli = fps_milli;
    }

    pub(crate) fn id(&self) -> u64 {
        match &self.body {
            Body::Local(lease) => lease.id(),
            Body::Remote(frame) => frame.lease().id(),
        }
    }

    pub(crate) fn timestamp_ns(&self) -> u64 {
        match &self.body {
            Body::Local(lease) => lease.timestamp_ns(),
            Body::Remote(frame) => frame.lease().timestamp_ns(),
        }
    }

    pub(crate) fn meta(&self) -> &FrameMetadata {
        &self.meta
    }

    fn memory(&self) -> &dyn BufferMemory {
        match &self.body {
            Body::Local(lease) => lease.memory(),
            Body::Remote(frame) => frame.lease().memory(),
        }
    }

    fn planes(&self) -> usize {
        match &self.body {
            Body::Local(lease) => lease.layouts().len(),
            Body::Remote(frame) => frame.lease().layouts().len(),
        }
    }

    /// The frame's identity, format and metadata.
    pub(crate) fn info(&self) -> FrameInfo {
        let (format, memory_kind) = match &self.body {
            Body::Local(lease) => (*lease.format(), lease.memory_kind()),
            Body::Remote(frame) => (*frame.lease().format(), frame.lease().memory_kind()),
        };
        FrameInfo {
            id: self.id(),
            timestamp_ns: self.timestamp_ns(),
            fourcc: format.code.to_u32(),
            width: format.resolution.width.get(),
            height: format.resolution.height.get(),
            planes: u32::try_from(self.planes()).unwrap_or(u32::MAX),
            fps_milli: self.fps_milli,
            exposure_us: self.meta.exposure_us,
            analogue_gain_milli: self.meta.analogue_gain_milli,
            memory_kind: match memory_kind {
                MemoryKind::Host => MEMORY_HOST,
                MemoryKind::Memfd => MEMORY_MEMFD,
                MemoryKind::Dmabuf => MEMORY_DMABUF,
                MemoryKind::Gpu => MEMORY_GPU,
            },
            achieved_fps_milli: self.meta.achieved_fps_milli,
        }
    }

    /// Plane `index`, its bytes valid until the frame is released.
    pub(crate) fn plane(&self, index: usize) -> Res<PlaneOut> {
        if index >= self.planes() {
            return Err(Fail::new(
                crate::status::E_NOT_FOUND,
                format!("the frame has {} planes", self.planes()),
            ));
        }
        let view = match &self.body {
            Body::Local(lease) => lease.read().and_then(|read| view(&read, index)),
            Body::Remote(frame) => frame.lease().read().and_then(|read| view(&read, index)),
        };
        view.ok_or_else(|| {
            Fail::new(
                E_UNSUPPORTED,
                "this frame's memory is not mapped for the CPU (read it through its descriptor)",
            )
        })
    }

    /// The metadata record in the producer's wire form (`styx_controls` version 1).
    pub(crate) fn metadata(&self) -> [u8; WIRE_BYTES] {
        let mut blob = [0_u8; WIRE_BYTES];
        let _ = self.meta.to_bytes(&mut blob);
        blob
    }

    /// The frame's descriptor, when its memory has one.
    pub(crate) fn dma(&self) -> Option<DmaView> {
        let memory = self.memory();
        let fd = memory.fd()?;
        Some(DmaView {
            fd: fd.as_raw_fd(),
            offset: u64::try_from(memory.fd_offset()).unwrap_or(u64::MAX),
            len: u64::try_from(memory.len()).unwrap_or(u64::MAX),
            kind: self.info().memory_kind,
        })
    }
}

/// Plane `index` of an open read, as a pointer that lives as long as the frame. The read's end
/// bracket runs when it drops here; the memory itself stays with the frame.
fn view<M>(read: &CpuRead<'_, M>, index: usize) -> Option<PlaneOut> {
    let plane = read.plane(index)?;
    Some(PlaneOut {
        data: plane.data.as_ptr(),
        len: plane.data.len(),
        stride: plane.stride,
    })
}
