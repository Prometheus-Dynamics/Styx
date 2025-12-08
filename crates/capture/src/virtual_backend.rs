//! Virtual capture backend that emits patterned frames from a buffer pool.
use std::sync::atomic::{AtomicU64, Ordering};

use styx_core::prelude::*;

use crate::{CaptureConfig, CaptureDescriptor, CaptureSource, Mode};

/// Simple virtual capture backend that emits patterned frames from a buffer pool.
///
/// # Example
/// ```rust
/// use styx_capture::prelude::*;
///
/// let pool = BufferPool::with_capacity(1, 128);
/// let res = Resolution::new(4, 4).unwrap();
/// let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
/// let mode = Mode {
///     id: ModeId { format, interval: None },
///     format,
///     intervals: smallvec::smallvec![],
///     interval_stepwise: None,
/// };
/// let source = VirtualCapture::new(mode, pool, 3);
/// let frame = source.next_frame().unwrap();
/// assert_eq!(frame.meta().format.code.to_string(), "RG24");
/// ```
pub struct VirtualCapture {
    descriptor: CaptureDescriptor,
    pool: BufferPool,
    mode: Mode,
    bytes_per_pixel: usize,
    counter: AtomicU64,
}

impl VirtualCapture {
    /// Create a virtual source using the provided mode and pool.
    pub fn new(mode: Mode, pool: BufferPool, bytes_per_pixel: usize) -> Self {
        let descriptor = CaptureDescriptor {
            modes: vec![mode.clone()],
            controls: Vec::new(),
        };
        Self {
            descriptor,
            pool,
            mode,
            bytes_per_pixel,
            counter: AtomicU64::new(0),
        }
    }

    fn next_payload(&self, timestamp: u64) -> FrameLease {
        crate::build_frame_from_pool(
            self.mode.format,
            &self.pool,
            timestamp,
            self.bytes_per_pixel,
        )
    }

    /// Emit a single frame and return whether it was accepted by the downstream queue.
    pub fn tick(&self, _config: &CaptureConfig, sink: &BoundedTx<FrameLease>) -> SendOutcome {
        let ts = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut frame = self.next_payload(ts);
        for mut plane in frame.planes_mut() {
            for byte in plane.data().iter_mut() {
                *byte = (ts % 256) as u8;
            }
        }
        sink.send(frame)
    }
}

impl CaptureSource for VirtualCapture {
    fn descriptor(&self) -> &CaptureDescriptor {
        &self.descriptor
    }

    fn next_frame(&self) -> Option<FrameLease> {
        let ts = self.counter.fetch_add(1, Ordering::Relaxed);
        Some(self.next_payload(ts))
    }
}
