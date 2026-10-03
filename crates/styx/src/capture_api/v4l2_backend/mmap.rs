//! The MMAP buffer queue of a V4L2 capture node, on `styx-kernel`.
//!
//! Buffers are mapped once and handed out by index; a buffer checked out to a frame is
//! requeued only when it is recycled. Streaming starts lazily on the first dequeue with every
//! free buffer queued. With [`V4l2MmapManager::new_imported`] the buffers are the caller's
//! dma-bufs (`V4L2_MEMORY_DMABUF`) instead.

use std::os::fd::OwnedFd;
use std::time::Duration;

use parking_lot::Mutex;
use styx_kernel::Mapping;
use styx_kernel::v4l2::{BufType, DequeuedBuffer, DmaBufAccess, Memory, QueueBuffer, VideoDevice};

use crate::capture_api::import::{CaptureBuffers, Claim};

pub(super) struct V4l2MmapManager {
    device: VideoDevice,
    buf_type: BufType,
    buffers: Vec<Mapping>,
    /// Imported dma-bufs (`Memory::DmaBuf`) in place of `buffers`.
    imported: Option<Claim>,
    state: Mutex<V4l2MmapState>,
}

struct V4l2MmapState {
    active: bool,
    queued: Vec<bool>,
    checked_out: Vec<bool>,
    timeout: Duration,
}

impl V4l2MmapManager {
    /// Allocates `count` MMAP buffers (the driver may choose another count) and maps them.
    pub(super) fn new(
        device: VideoDevice,
        buf_type: BufType,
        count: u32,
        timeout: Duration,
    ) -> styx_kernel::Result<Self> {
        let granted = device.request_buffers(buf_type, Memory::Mmap, count)?.count;
        let mut buffers = Vec::with_capacity(granted as usize);
        for index in 0..granted {
            let plane = device
                .map_buffer(buf_type, index)?
                .into_iter()
                .next()
                .ok_or_else(|| styx_kernel::Error::Invalid("v4l2 buffer has no plane".into()))?;
            buffers.push(plane);
        }
        Ok(Self::with_buffers(
            device, buf_type, buffers, None, granted, timeout,
        ))
    }

    fn with_buffers(
        device: VideoDevice,
        buf_type: BufType,
        buffers: Vec<Mapping>,
        imported: Option<Claim>,
        count: u32,
        timeout: Duration,
    ) -> Self {
        Self {
            device,
            buf_type,
            buffers,
            imported,
            state: Mutex::new(V4l2MmapState {
                active: false,
                queued: vec![false; count as usize],
                checked_out: vec![false; count as usize],
                timeout,
            }),
        }
    }

    /// Captures into the caller's dma-bufs, one V4L2 buffer per buffer. Gives the device back
    /// when the driver does not import them (e.g. v4l2loopback: MMAP only) or they are not all
    /// dma-bufs, so the caller can fall back to [`Self::new`].
    pub(super) fn new_imported(
        device: VideoDevice,
        buf_type: BufType,
        buffers: &CaptureBuffers,
        timeout: Duration,
    ) -> Result<Self, Box<(VideoDevice, String)>> {
        if !(0..buffers.len()).all(|i| buffers.is_dmabuf(i)) {
            return Err(Box::new((device, "buffers are not dma-bufs".into())));
        }
        let Some(claim) = buffers.claim() else {
            return Err(Box::new((
                device,
                "buffers are in use by another capture".into(),
            )));
        };
        let count = buffers.len() as u32;
        match device.request_buffers(buf_type, Memory::DmaBuf, count) {
            Ok(granted) if granted.count == count => {}
            Ok(granted) => {
                let _ = device.free_buffers(buf_type, Memory::DmaBuf);
                return Err(Box::new((
                    device,
                    format!("driver wants {} buffers", granted.count),
                )));
            }
            Err(err) => return Err(Box::new((device, format!("no DMABUF import: {err}")))),
        }
        Ok(Self::with_buffers(
            device,
            buf_type,
            Vec::new(),
            Some(claim),
            count,
            timeout,
        ))
    }

    /// The caller's buffers, when capturing into them.
    pub(super) fn imported(&self) -> Option<&CaptureBuffers> {
        self.imported.as_ref().map(Claim::buffers)
    }

    fn memory(&self) -> Memory {
        match self.imported {
            Some(_) => Memory::DmaBuf,
            None => Memory::Mmap,
        }
    }

    /// Waits up to the poll timeout for a filled buffer and dequeues it; `Ok(None)` when none
    /// arrived. Starts streaming first when needed.
    pub(super) fn dequeue(&self) -> styx_kernel::Result<Option<(usize, DequeuedBuffer)>> {
        let timeout = {
            let mut state = self.state.lock();
            if !state.active {
                for index in 0..state.queued.len() {
                    if !state.queued[index] && !state.checked_out[index] {
                        self.queue_locked(index, &mut state)?;
                    }
                }
                self.stream_on_locked(&mut state)?;
            }
            state.timeout
        };

        if self.device.wait_readable(Some(timeout))?.is_empty() {
            return Ok(None);
        }
        let Some(buf) = self.device.dequeue(self.buf_type, self.memory())? else {
            return Ok(None);
        };
        let index = buf.index as usize;
        let mut state = self.state.lock();
        if index < state.queued.len() {
            state.queued[index] = false;
            state.checked_out[index] = true;
        }
        Ok(Some((index, buf)))
    }

    pub(super) fn recycle(&self, index: usize) -> styx_kernel::Result<()> {
        let mut state = self.state.lock();
        if index >= state.checked_out.len() {
            return Ok(());
        }
        state.checked_out[index] = false;
        if state.active {
            self.queue_locked(index, &mut state)?;
        }
        Ok(())
    }

    pub(super) fn mapped_plane(&self, index: usize) -> Option<&[u8]> {
        if let Some(imported) = self.imported() {
            return imported.bytes(index);
        }
        self.buffers.get(index).map(Mapping::as_slice)
    }

    pub(super) fn mapped_bytes(&self, index: usize) -> Option<usize> {
        self.mapped_plane(index).map(<[u8]>::len)
    }

    /// Exports a buffer as a read-only dma-buf.
    pub(super) fn export_dmabuf(&self, index: usize) -> std::io::Result<OwnedFd> {
        if let Some(imported) = self.imported() {
            return imported
                .fd(index)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))?
                .try_clone_to_owned();
        }
        self.device
            .export_buffer_with(self.buf_type, index as u32, 0, DmaBufAccess::ReadOnly)
            .map_err(to_io)
    }

    pub(super) fn stop_stream(&self) -> styx_kernel::Result<()> {
        let mut state = self.state.lock();
        self.stop_stream_locked(&mut state)
    }

    fn stream_on_locked(&self, state: &mut V4l2MmapState) -> styx_kernel::Result<()> {
        if state.active {
            return Ok(());
        }
        self.device.stream_on(self.buf_type)?;
        state.active = true;
        Ok(())
    }

    fn stop_stream_locked(&self, state: &mut V4l2MmapState) -> styx_kernel::Result<()> {
        if !state.active {
            return Ok(());
        }
        self.device.stream_off(self.buf_type)?;
        state.active = false;
        for queued in &mut state.queued {
            *queued = false;
        }
        Ok(())
    }

    fn queue_locked(&self, index: usize, state: &mut V4l2MmapState) -> styx_kernel::Result<()> {
        if state.queued[index] {
            return Ok(());
        }
        match self.imported().and_then(|b| b.fd(index)) {
            Some(fd) => {
                self.device
                    .queue(&QueueBuffer::dmabuf(self.buf_type, index as u32, &[fd]))?
            }
            None => self
                .device
                .queue(&QueueBuffer::mmap(self.buf_type, index as u32))?,
        }
        state.queued[index] = true;
        Ok(())
    }
}

impl Drop for V4l2MmapManager {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        let _ = self.stop_stream_locked(&mut state);
        drop(state);
        // Unmap before freeing the buffers so the driver can release them at once.
        self.buffers.clear();
        let _ = self.device.free_buffers(self.buf_type, self.memory());
    }
}

/// Converts a kernel error to the `io::Error` of the failed call's errno.
pub(super) fn to_io(err: styx_kernel::Error) -> std::io::Error {
    match err.errno() {
        Some(errno) => std::io::Error::from_raw_os_error(errno),
        None => std::io::Error::other(err),
    }
}
