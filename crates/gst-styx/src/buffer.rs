//! Styx frames as GStreamer buffers, without copying: camera dma-bufs become
//! `GstDmaBufMemory`, other frames wrap the frame's own memory. The frame (and with it the camera
//! buffer) is released when GStreamer frees the last memory referring to it.

use std::sync::Arc;

use gst::glib::translate::IntoGlib;
use gst_allocators::prelude::*;
use styx::prelude::*;

use crate::caps::video_format;

/// One plane of a frame, readable as a slice while the frame is alive.
struct PlaneMemory {
    frame: Arc<FrameLease>,
    plane: usize,
}

impl AsRef<[u8]> for PlaneMemory {
    fn as_ref(&self) -> &[u8] {
        self.frame
            .planes()
            .get(self.plane)
            .map(|p| p.data())
            .unwrap_or(&[])
    }
}

/// What memory the buffer should have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    /// Wrap the frame's own memory (mapped camera buffer, memfd or heap).
    Wrap,
    /// dma-bufs when the frame's memory can be exported, wrapped memory otherwise.
    PreferDmaBuf,
    /// dma-bufs only.
    DmaBuf,
}

/// How a frame became a buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backing {
    /// The frame's own memory, wrapped.
    Wrapped,
    /// Exported dma-bufs.
    DmaBuf,
    /// Wrapped because exporting failed (with [`MemoryKind::PreferDmaBuf`]), and why.
    Fallback(String),
}

/// Why a frame could not become a buffer.
#[derive(Debug)]
pub enum BufferError {
    /// The frame has no readable planes.
    Empty,
    /// dma-buf memory was required but the frame's memory cannot be exported.
    NotExportable(String),
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "frame has no readable planes"),
            Self::NotExportable(why) => write!(f, "frame is not a dma-buf: {why}"),
        }
    }
}

/// Make a buffer from `frame` with `kind` memory. Raw formats carry a `GstVideoMeta` with the
/// frame's strides and offsets.
pub fn frame_buffer(
    frame: FrameLease,
    kind: MemoryKind,
) -> Result<(gst::Buffer, Backing), BufferError> {
    let frame = Arc::new(frame);
    match kind {
        MemoryKind::Wrap => wrapped_buffer(&frame).map(|buffer| (buffer, Backing::Wrapped)),
        MemoryKind::DmaBuf => dmabuf_buffer(&frame)
            .map(|buffer| (buffer, Backing::DmaBuf))
            .map_err(BufferError::NotExportable),
        MemoryKind::PreferDmaBuf => match dmabuf_buffer(&frame) {
            Ok(buffer) => Ok((buffer, Backing::DmaBuf)),
            Err(why) => wrapped_buffer(&frame).map(|buffer| (buffer, Backing::Fallback(why))),
        },
    }
}

fn wrapped_buffer(frame: &Arc<FrameLease>) -> Result<gst::Buffer, BufferError> {
    let planes = frame.planes();
    if planes.is_empty() || planes.iter().all(|p| p.data().is_empty()) {
        return Err(BufferError::Empty);
    }
    let mut buffer = gst::Buffer::new();
    let mut offsets = Vec::with_capacity(planes.len());
    let mut strides = Vec::with_capacity(planes.len());
    let mut total = 0usize;
    {
        let buffer = buffer.get_mut().expect("new buffer is writable");
        for (index, plane) in planes.iter().enumerate() {
            offsets.push(total);
            strides.push(plane.stride() as i32);
            total += plane.data().len();
            buffer.append_memory(gst::Memory::from_slice(PlaneMemory {
                frame: frame.clone(),
                plane: index,
            }));
        }
        add_video_meta(buffer, frame, &offsets, &strides);
    }
    Ok(buffer)
}

fn dmabuf_buffer(frame: &Arc<FrameLease>) -> Result<gst::Buffer, String> {
    let FrameBackingExport::DmabufPlanes { planes: fds } =
        frame.export_backing().map_err(|e| e.to_string())?
    else {
        return Err("frame memory is a memfd, not a dma-buf".into());
    };
    let layouts = frame.layouts();
    // One dma-buf for the whole frame (V4L2 single-planar, native): plane offsets are offsets
    // into it. One per plane (libcamera): offsets are within each plane's memory.
    let per_plane = match fds.len() {
        1 => false,
        n if n == layouts.len() => true,
        n => return Err(format!("{n} dma-bufs for {} planes", layouts.len())),
    };
    let allocator = gst_allocators::DmaBufAllocator::new();
    let mut buffer = gst::Buffer::new();
    let mut offsets = Vec::with_capacity(layouts.len());
    let strides: Vec<i32> = layouts.iter().map(|l| l.stride as i32).collect();
    {
        let buffer = buffer.get_mut().expect("new buffer is writable");
        let mut base = 0usize;
        for (index, plane) in fds.into_iter().enumerate() {
            let span = plane.offset + plane.len;
            // SAFETY: `plane.fd` is a dma-buf file descriptor this function owns (exported for
            // this frame); the allocator takes ownership and closes it with the memory.
            let mut memory =
                unsafe { allocator.alloc_dmabuf(plane.fd, span) }.map_err(|e| e.to_string())?;
            {
                let memory = memory.get_mut().expect("new memory is writable");
                memory.resize(plane.offset..span);
                keep_alive(memory, frame.clone());
            }
            if per_plane {
                offsets.push(base + layouts[index].offset);
            }
            base += plane.len;
            buffer.append_memory(memory);
        }
        if !per_plane {
            offsets.extend(layouts.iter().map(|l| l.offset));
        }
        add_video_meta(buffer, frame, &offsets, &strides);
    }
    Ok(buffer)
}

fn add_video_meta(
    buffer: &mut gst::BufferRef,
    frame: &FrameLease,
    offsets: &[usize],
    strides: &[i32],
) {
    let format = frame.meta().format;
    let Some(video) = video_format(format.code) else {
        return; // compressed: no video meta
    };
    let res = format.resolution;
    let _ = gst_video::VideoMeta::add_full(
        buffer,
        gst_video::VideoFrameFlags::empty(),
        video,
        res.width.get(),
        res.height.get(),
        offsets,
        strides,
    );
}

/// Keep `frame` alive as long as `memory` exists (GStreamer may hand memories to other buffers).
fn keep_alive(memory: &mut gst::MemoryRef, frame: Arc<FrameLease>) {
    unsafe extern "C" fn release(data: gst::glib::ffi::gpointer) {
        // SAFETY: `data` is the `Box<Arc<FrameLease>>` leaked below; GStreamer calls this
        // destroy notify exactly once.
        drop(unsafe { Box::from_raw(data as *mut Arc<FrameLease>) });
    }
    let quark = gst::glib::Quark::from_str("styx-frame");
    let data = Box::into_raw(Box::new(frame));
    // SAFETY: the memory is a valid, writable mini object; the qdata takes ownership of `data`
    // and frees it with `release` when the memory is finalized.
    unsafe {
        gst::ffi::gst_mini_object_set_qdata(
            memory.as_mut_ptr() as *mut gst::ffi::GstMiniObject,
            quark.into_glib(),
            data as gst::glib::ffi::gpointer,
            Some(release),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn virtual_frame() -> FrameLease {
        let device = CaptureRequest::virtual_source(
            VirtualSourceConfig::new()
                .name("virtual")
                .resolution(64, 48)
                .fps(30),
        )
        .into_device();
        let handle = CaptureRequest::new(&device).start().unwrap();
        loop {
            if let RecvOutcome::Data(frame) = handle.recv_blocking(Duration::from_millis(100)) {
                handle.stop();
                return frame;
            }
        }
    }

    #[test]
    fn wrapped_frame_is_zero_copy_with_video_meta() {
        gst::init().unwrap();
        let frame = virtual_frame();
        let first = frame.planes()[0].data().as_ptr();
        let stride = frame.planes()[0].stride();
        let (buffer, backing) = frame_buffer(frame, MemoryKind::Wrap).unwrap();
        assert_eq!(backing, Backing::Wrapped);
        let map = buffer.map_readable().unwrap();
        assert_eq!(map.as_ptr(), first, "buffer maps the frame's own memory");
        let meta = buffer.meta::<gst_video::VideoMeta>().unwrap();
        assert_eq!(meta.format(), gst_video::VideoFormat::Rgb);
        assert_eq!((meta.width(), meta.height()), (64, 48));
        assert_eq!(meta.stride()[0] as usize, stride);
    }

    #[test]
    fn heap_frames_are_not_dma_bufs() {
        gst::init().unwrap();
        assert!(matches!(
            frame_buffer(virtual_frame(), MemoryKind::DmaBuf),
            Err(BufferError::NotExportable(_))
        ));
        let (_, backing) = frame_buffer(virtual_frame(), MemoryKind::PreferDmaBuf).unwrap();
        assert!(matches!(backing, Backing::Fallback(_)));
    }
}
