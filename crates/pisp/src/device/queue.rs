//! An MMAP buffer queue on one video node.

use std::time::{Duration, Instant};

use styx_kernel::Mapping;
use styx_kernel::v4l2::{BufType, DequeuedBuffer, Memory, QueueBuffer, QueuePlane, VideoDevice};

use super::{DeviceError, Result};

/// A video node with MMAP buffers, all mapped.
pub struct Queue {
    /// The node.
    pub dev: VideoDevice,
    /// Its queue type.
    pub buf_type: BufType,
    /// Mappings per buffer, per plane.
    pub maps: Vec<Vec<Mapping>>,
    name: &'static str,
}

impl Queue {
    /// Requests `count` MMAP buffers on `dev` and maps them.
    pub fn new(
        dev: VideoDevice,
        buf_type: BufType,
        count: u32,
        name: &'static str,
    ) -> Result<Self> {
        let got = dev.request_buffers(buf_type, Memory::Mmap, count)?;
        if got.count == 0 {
            return Err(DeviceError::Setup(format!("{name}: no buffers")));
        }
        let maps = (0..got.count)
            .map(|i| dev.map_buffer(buf_type, i))
            .collect::<styx_kernel::Result<Vec<_>>>()?;
        Ok(Self {
            dev,
            buf_type,
            maps,
            name,
        })
    }

    /// Number of buffers.
    pub fn len(&self) -> usize {
        self.maps.len()
    }

    /// Whether there are no buffers.
    pub fn is_empty(&self) -> bool {
        self.maps.is_empty()
    }

    /// Queues buffer `index`; for output queues `bytes_used` gives each plane's payload.
    pub fn queue(&self, index: u32, bytes_used: &[u32]) -> Result<()> {
        let nplanes = self.maps[index as usize].len();
        let mut q = QueueBuffer::mmap(self.buf_type, index);
        if self.buf_type.is_multiplanar() || !bytes_used.is_empty() {
            q.planes = (0..nplanes)
                .map(|p| QueuePlane {
                    bytes_used: bytes_used.get(p).copied().unwrap_or(0),
                    ..Default::default()
                })
                .collect();
        }
        super::profile::time(self.name, "qbuf", || self.dev.queue(&q))?;
        Ok(())
    }

    /// Dequeues a buffer, waiting up to `timeout`.
    pub fn dequeue(&self, timeout: Duration) -> Result<DequeuedBuffer> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(b) = super::profile::time(self.name, "dqbuf", || {
                self.dev.dequeue(self.buf_type, Memory::Mmap)
            })? {
                return Ok(b);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(DeviceError::Timeout(self.name));
            }
            super::profile::time(self.name, "poll", || self.dev.wait(Some(left)))?;
        }
    }

    /// Starts streaming.
    pub fn stream_on(&self) -> Result<()> {
        Ok(self.dev.stream_on(self.buf_type)?)
    }

    /// Stops streaming and frees the buffers (mappings are dropped first).
    pub fn close(mut self) -> Result<()> {
        let r = self.dev.stream_off(self.buf_type);
        self.maps.clear();
        let f = self.dev.free_buffers(self.buf_type, Memory::Mmap);
        r?;
        f?;
        Ok(())
    }
}
