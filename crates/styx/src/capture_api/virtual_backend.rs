use std::thread;
use std::time::Duration;

use styx_capture::CaptureSource;
use styx_capture::virtual_backend::VirtualCapture;
use styx_core::prelude::*;

use crate::BackendKind;
use crate::capture_api::handle::{ControlPlane, WorkerHandle};
use crate::capture_api::{CaptureDescriptor, CaptureError, CaptureHandle};
use crate::metrics::StageMetrics;
use crate::prelude::{Interval, Mode};

pub(super) fn start_virtual(
    mode: Mode,
    interval: Option<Interval>,
    descriptor: CaptureDescriptor,
) -> Result<CaptureHandle, CaptureError> {
    let (pool_min, pool_bytes, pool_spare) = crate::capture_api::capture_pool_limits(4, 1 << 20, 8);
    let pool = BufferPool::with_limits(pool_min, pool_bytes, pool_spare);
    let capture = VirtualCapture::new(mode.clone(), pool, 3);
    let queue_depth = crate::capture_api::capture_queue_depth();
    let (tx, rx) = bounded(queue_depth);
    let worker = thread::spawn(move || {
        loop {
            if let Some(frame) = capture.next_frame() {
                if matches!(tx.send(frame), SendOutcome::Closed) {
                    break;
                }
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        }
    });
    Ok(CaptureHandle {
        backend: BackendKind::Virtual,
        control: ControlPlane::Virtual,
        descriptor,
        mode,
        interval,
        rx,
        stop_tx: None,
        worker: Some(WorkerHandle::Thread(worker)),
        metrics: StageMetrics::default(),
    })
}
