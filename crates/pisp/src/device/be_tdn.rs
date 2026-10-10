//! The temporal denoise buffers of a back end node group: two dma-bufs of the input's format,
//! one written as the new long-term average (`pispbe-tdn_output`) while the other, written by
//! the previous job, is read (`pispbe-tdn_input`); they swap every job, as libcamera's PiSP
//! pipeline handler does.

use std::os::fd::{AsFd, OwnedFd};
use std::time::{Duration, Instant};

use styx_kernel::FourCc;
use styx_kernel::dma_heap::DmaHeap;
use styx_kernel::v4l2::{BufType, Format, Memory, Planes, QueueBuffer, QueuePlane, VideoDevice};

use super::{DeviceError, Result};
use crate::uapi::bayer_enable;

/// Heaps tried for the buffers (the CPU never touches them).
const HEAPS: [&str; 2] = ["linux,cma", "system"];

const IN: BufType = BufType::VideoOutputMplane;
const OUT: BufType = BufType::VideoCaptureMplane;

pub(super) struct TdnBuffers {
    input: VideoDevice,
    output: VideoDevice,
    fds: [OwnedFd; 2],
    len: u32,
    /// The buffer the last job wrote (the long-term average to read next).
    last_written: Option<usize>,
    /// What the job in flight queued: (read slot, written slot).
    in_flight: (Option<usize>, Option<usize>),
}

fn set_format(
    dev: &VideoDevice,
    ty: BufType,
    w: u32,
    h: u32,
    fourcc: FourCc,
    stride: u32,
) -> Result<()> {
    let fmt = super::be_stream::mplane(w, h, fourcc, stride);
    let Format::Multi(g) = dev.set_format(ty, &fmt)? else {
        return Err(DeviceError::Setup("TDN node is not multi-planar".into()));
    };
    if (g.width, g.height) != (w, h) || g.planes.first().map(|p| p.bytes_per_line) != Some(stride) {
        return Err(DeviceError::Setup(format!(
            "TDN node gave {}x{} planes {:?}",
            g.width, g.height, g.planes
        )));
    }
    Ok(())
}

impl TdnBuffers {
    /// Sets both nodes to the input's format (`w`x`h`, 16-bit Bayer `fourcc`, `stride`),
    /// allocates and imports two buffers and starts streaming.
    pub(super) fn new(
        input: VideoDevice,
        output: VideoDevice,
        (w, h, fourcc, stride): (u32, u32, FourCc, u32),
    ) -> Result<Self> {
        set_format(&input, IN, w, h, fourcc, stride)?;
        set_format(&output, OUT, w, h, fourcc, stride)?;
        let heap = HEAPS
            .iter()
            .find_map(|n| DmaHeap::open(n).ok())
            .ok_or_else(|| DeviceError::Setup("no dma-heap for the TDN buffers".into()))?;
        let len = (stride as usize * h as usize).next_multiple_of(4096);
        let fds = [heap.allocate(len)?.into_fd(), heap.allocate(len)?.into_fd()];
        for (dev, ty) in [(&input, IN), (&output, OUT)] {
            let got = dev.request_buffers(ty, Memory::DmaBuf, 2)?;
            if got.count < 2 {
                let _ = dev.free_buffers(ty, Memory::DmaBuf);
                return Err(DeviceError::Setup(format!(
                    "TDN node gave {} of 2 buffer slots",
                    got.count
                )));
            }
        }
        input.stream_on(IN)?;
        if let Err(e) = output.stream_on(OUT) {
            let _ = input.stream_off(IN);
            return Err(e.into());
        }
        Ok(Self {
            input,
            output,
            fds,
            len: len as u32,
            last_written: None,
            in_flight: (None, None),
        })
    }

    /// Forget the long-term average (the next job must not read it).
    pub(super) fn reset(&mut self) {
        self.last_written = None;
    }

    /// Queues the buffers a job with `bayer_enables` uses.
    pub(super) fn queue(&mut self, bayer_enables: u32) -> Result<()> {
        let write = bayer_enables & bayer_enable::TDN_OUTPUT != 0;
        let read = bayer_enables & bayer_enable::TDN_INPUT != 0;
        let r = if read {
            Some(self.last_written.ok_or_else(|| {
                DeviceError::Setup("TDN input enabled with no average written yet".into())
            })?)
        } else {
            None
        };
        let w = write.then(|| self.last_written.map_or(0, |l| 1 - l));
        if let Some(slot) = r {
            self.queue_one(&self.input, IN, slot)?;
        }
        if let Some(slot) = w {
            self.queue_one(&self.output, OUT, slot)?;
        }
        self.in_flight = (r, w);
        Ok(())
    }

    fn queue_one(&self, dev: &VideoDevice, ty: BufType, slot: usize) -> Result<()> {
        let fd = self.fds[slot].as_fd();
        let mut q = QueueBuffer::dmabuf(ty, slot as u32, &[fd]);
        q.planes = Planes::one(QueuePlane {
            dmabuf: Some(fd),
            length: self.len,
            bytes_used: if ty == IN { self.len } else { 0 },
            data_offset: 0,
        });
        super::profile::time("pispbe-tdn", "qbuf", || dev.queue(&q))?;
        Ok(())
    }

    /// Dequeues what [`Self::queue`] queued once the job is done; the written buffer is the
    /// next one read.
    pub(super) fn complete(&mut self, timeout: Duration) -> Result<()> {
        let (r, w) = std::mem::take(&mut self.in_flight);
        if r.is_some() {
            dequeue(&self.input, IN, timeout)?;
        }
        if w.is_some() {
            dequeue(&self.output, OUT, timeout)?;
            self.last_written = w;
        }
        Ok(())
    }

    /// Drops a job that failed: nothing it wrote is trusted.
    pub(super) fn abandon(&mut self) {
        self.in_flight = (None, None);
        self.last_written = None;
    }

    pub(super) fn close(self) -> Result<()> {
        let a = self.input.stream_off(IN);
        let b = self.output.stream_off(OUT);
        let c = self.input.free_buffers(IN, Memory::DmaBuf);
        let d = self.output.free_buffers(OUT, Memory::DmaBuf);
        a?;
        b?;
        c?;
        d?;
        Ok(())
    }
}

fn dequeue(dev: &VideoDevice, ty: BufType, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if dev.dequeue(ty, Memory::DmaBuf)?.is_some() {
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(DeviceError::Timeout("pispbe-tdn"));
        }
        dev.wait(Some(left))?;
    }
}
