//! An MMAP buffer queue on one video node, every buffer mapped ([`MmapQueue`]).

use std::time::{Duration, Instant};

use super::{BufType, DequeuedBuffer, Memory, Planes, QueueBuffer, QueuePlane, VideoDevice};
use crate::{Error, Mapping, Result};

/// Records how long an operation on a queue took: `(queue name, operation, time)`, with the
/// operations `qbuf`, `dqbuf` and `poll`.
pub type QueueTimer = fn(&'static str, &'static str, Duration);

/// A video node with MMAP buffers, all mapped: the ISP nodes' parameter, statistics and image
/// queues.
pub struct MmapQueue {
    /// The node.
    pub dev: VideoDevice,
    /// Its queue type.
    pub buf_type: BufType,
    /// Mappings per buffer, per plane.
    pub maps: Vec<Vec<Mapping>>,
    /// The queue's name, for errors and the timer.
    pub name: &'static str,
    timer: Option<QueueTimer>,
}

impl MmapQueue {
    /// Requests `count` MMAP buffers on `dev` and maps them; an error if the driver gives none.
    pub fn new(
        dev: VideoDevice,
        buf_type: BufType,
        count: u32,
        name: &'static str,
    ) -> Result<Self> {
        let got = dev.request_buffers(buf_type, Memory::Mmap, count)?;
        if got.count == 0 {
            return Err(Error::Invalid(format!("{name}: no buffers")));
        }
        let maps = (0..got.count)
            .map(|i| dev.map_buffer(buf_type, i))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            dev,
            buf_type,
            maps,
            name,
            timer: None,
        })
    }

    /// Times `qbuf`, `dqbuf` and `poll` with `timer`.
    pub fn with_timer(mut self, timer: QueueTimer) -> Self {
        self.timer = Some(timer);
        self
    }

    fn timed<T>(&self, op: &'static str, f: impl FnOnce() -> T) -> T {
        match self.timer {
            Some(timer) => {
                let t = Instant::now();
                let r = f();
                timer(self.name, op, t.elapsed());
                r
            }
            None => f(),
        }
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
                .collect::<Planes<_>>();
        }
        self.timed("qbuf", || self.dev.queue(&q))
    }

    /// Dequeues a buffer if one is ready.
    pub fn try_dequeue(&self) -> Result<Option<DequeuedBuffer>> {
        self.timed("dqbuf", || self.dev.dequeue(self.buf_type, Memory::Mmap))
    }

    /// Dequeues a buffer, waiting up to `timeout`; `None` if none came.
    pub fn dequeue_within(&self, timeout: Duration) -> Result<Option<DequeuedBuffer>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(b) = self.try_dequeue()? {
                return Ok(Some(b));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            self.timed("poll", || self.dev.wait(Some(left)))?;
        }
    }

    /// Starts streaming.
    pub fn stream_on(&self) -> Result<()> {
        self.dev.stream_on(self.buf_type)
    }

    /// Stops streaming and frees the buffers (mappings are dropped first).
    pub fn close(mut self) -> Result<()> {
        let r = self.dev.stream_off(self.buf_type);
        self.maps.clear();
        let f = self.dev.free_buffers(self.buf_type, Memory::Mmap);
        r?;
        f
    }
}
