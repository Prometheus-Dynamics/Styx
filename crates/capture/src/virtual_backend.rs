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
/// let format = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
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
    storage: VirtualStorage,
    mode: Mode,
    bytes_per_pixel: usize,
    counter: AtomicU64,
}

enum VirtualStorage {
    Owned(BufferPool),
    #[cfg(target_os = "linux")]
    Shared(SharedBufferPool),
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
            storage: VirtualStorage::Owned(pool),
            mode,
            bytes_per_pixel,
            counter: AtomicU64::new(0),
        }
    }

    #[cfg(target_os = "linux")]
    pub fn new_shared(mode: Mode, pool: SharedBufferPool, bytes_per_pixel: usize) -> Self {
        let descriptor = CaptureDescriptor {
            modes: vec![mode.clone()],
            controls: Vec::new(),
        };
        Self {
            descriptor,
            storage: VirtualStorage::Shared(pool),
            mode,
            bytes_per_pixel,
            counter: AtomicU64::new(0),
        }
    }

    fn next_payload(&self, timestamp: u64, fill: Option<u8>) -> Option<FrameLease> {
        match &self.storage {
            VirtualStorage::Owned(pool) => {
                let mut frame = crate::build_frame_from_pool(
                    self.mode.format,
                    pool,
                    timestamp,
                    self.bytes_per_pixel,
                );
                if let Some(value) = fill {
                    for mut plane in frame.planes_mut() {
                        for byte in plane.data().iter_mut() {
                            *byte = value;
                        }
                    }
                }
                Some(frame)
            }
            #[cfg(target_os = "linux")]
            VirtualStorage::Shared(pool) => {
                let layout = plane_layout_from_dims(
                    self.mode.format.resolution.width,
                    self.mode.format.resolution.height,
                    self.bytes_per_pixel,
                );
                let mut lease = pool.lease().ok()?;
                lease.try_resize(layout.len).ok()?;
                if let Some(value) = fill {
                    for byte in lease.as_mut_slice() {
                        *byte = value;
                    }
                }
                let meta = FrameMeta::new(self.mode.format, timestamp)
                    .with_capture_instant(std::time::Instant::now())
                    .with_transition(ResidencyTransition {
                        from: FrameResidency::HostExternal,
                        to: FrameResidency::HostExternal,
                        reason: ResidencyTransitionReason::Capture,
                        copied: false,
                    });
                FrameLease::single_plane_shared(meta, lease, layout.len, layout.stride).ok()
            }
        }
    }

    /// Emit a single frame and return whether it was accepted by the downstream queue.
    pub fn tick(&self, _config: &CaptureConfig, sink: &BoundedTx<FrameLease>) -> SendOutcome {
        let ts = self.counter.fetch_add(1, Ordering::Relaxed);
        match self.next_payload(ts, Some((ts % 256) as u8)) {
            Some(frame) => sink.send(frame),
            None => SendOutcome::Full,
        }
    }
}

impl CaptureSource for VirtualCapture {
    fn descriptor(&self) -> &CaptureDescriptor {
        &self.descriptor
    }

    fn next_frame(&self) -> Option<FrameLease> {
        let ts = self.counter.fetch_add(1, Ordering::Relaxed);
        self.next_payload(ts, None)
    }
}
