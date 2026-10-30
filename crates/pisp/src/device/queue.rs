//! An MMAP buffer queue on one video node: styx-kernel's [`MmapQueue`], timed into the
//! profile, with a timeout as an error.

use std::time::Duration;

use styx_kernel::v4l2::{DequeuedBuffer, MmapQueue};

use super::{DeviceError, Result};

/// A video node with MMAP buffers, all mapped (its fields through [`MmapQueue`]).
pub struct Queue(MmapQueue);

impl std::ops::Deref for Queue {
    type Target = MmapQueue;

    fn deref(&self) -> &MmapQueue {
        &self.0
    }
}

impl std::ops::DerefMut for Queue {
    fn deref_mut(&mut self) -> &mut MmapQueue {
        &mut self.0
    }
}

impl Queue {
    /// Requests `count` MMAP buffers on `dev` and maps them.
    pub fn new(
        dev: styx_kernel::v4l2::VideoDevice,
        buf_type: styx_kernel::v4l2::BufType,
        count: u32,
        name: &'static str,
    ) -> Result<Self> {
        Ok(Self(
            MmapQueue::new(dev, buf_type, count, name)?.with_timer(super::profile::record_if_on),
        ))
    }

    /// Dequeues a buffer, waiting up to `timeout`.
    pub fn dequeue(&self, timeout: Duration) -> Result<DequeuedBuffer> {
        self.0
            .dequeue_within(timeout)?
            .ok_or(DeviceError::Timeout(self.0.name))
    }

    /// Stops streaming and frees the buffers (mappings are dropped first).
    pub fn close(self) -> Result<()> {
        Ok(self.0.close()?)
    }
}
