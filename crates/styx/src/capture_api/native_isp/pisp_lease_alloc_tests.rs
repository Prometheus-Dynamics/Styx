//! A native PiSP capture's frames reach a consumer in the capturing process without a copy and
//! without allocating per frame, besides each frame's lease record: the worker's steps (lease
//! a back end output buffer, deliver it to the consumer queue) and the consumer's (take it,
//! read it, drop it so the buffer goes back) on one thread, the back end standing in as memfd
//! output buffers, over many frames after a warm-up, counted with the test allocator.

use std::time::{Duration, Instant};

use styx_core::prelude::{FrameHops, Hop, RecvOutcome};

use super::super::super::handle::CaptureHandle;
use super::super::super::handle_metrics::deliver;
use super::tests::{buffer, frame};
use super::*;
use crate::test_alloc::allocations;

const W: usize = 640;
const H: usize = 400;
const BUFFERS: usize = 6;
const FRAMES: u64 = 500;

fn handle(rx: styx_core::queue::BoundedRx<FrameLease>, live: CaptureMetrics) -> CaptureHandle {
    use crate::BackendKind;
    use crate::capture_api::{CaptureDescriptor, ControlPlane};
    CaptureHandle {
        backend: BackendKind::Native,
        control: ControlPlane::Virtual,
        descriptor: CaptureDescriptor::new([]),
        mode: crate::prelude::Mode::new(
            MediaFormat::srgb(FourCc::NV12, W as u32, H as u32).expect("format"),
        ),
        interval: None,
        rx,
        stop_tx: None,
        worker: None,
        aux_workers: Vec::new(),
        #[cfg(feature = "libcamera")]
        libcamera_idle_stop_allowed: false,
        #[cfg(feature = "libcamera")]
        libcamera_stop_when_idle: false,
        metrics: Default::default(),
        external_backings: Vec::new(),
        worker_error: Default::default(),
        control_error: Default::default(),
        shutdown_stats: Default::default(),
        retry_metrics: Default::default(),
        sequence_gaps: Default::default(),
        live,
    }
}

#[test]
#[allow(clippy::print_stdout)]
fn pisp_frames_reach_an_in_process_consumer_without_allocating_or_copying() {
    let live = CaptureMetrics::default();
    let (queue_tx, queue_rx) = styx_core::queue::bounded(2);
    let handle = handle(queue_rx, live.clone());
    let (ret_tx, ret_rx) = returns();
    let buffers: Vec<BeBuffer> = (0..BUFFERS).map(|_| buffer(W, H)).collect();
    let mut free: Vec<u32> = (0..BUFFERS as u32).collect();
    let spec = OutputSpec {
        code: FourCc::NV12,
        width: W as u32,
        height: H as u32,
    };
    let placed = Placed {
        spec,
        stride: W,
        size: None,
        crop: None,
    };
    let mut f = frame();
    let (mut lease_allocs, mut path_allocs) = (0u64, 0u64);
    let mut xor = 0u8;
    for n in 0..FRAMES + 100 {
        let measured = n >= 100;
        // The worker: buffers back from consumers, the "back end" done, a lease delivered.
        let ((), returned) = allocations(|| {
            while let RecvOutcome::Data((_, index)) = ret_rx.recv() {
                free.push(index);
            }
        });
        let index = free.pop().expect("a free output buffer");
        let now = Instant::now();
        f.sequence = n;
        f.timestamp = Duration::from_nanos(CaptureInstant::from(now).as_nanos() - 8_000_000);
        f.dequeued = now - Duration::from_millis(3);
        f.times.total = Duration::from_millis(2);
        let (frame, leased) = allocations(|| {
            lease(
                placed,
                buffers[index as usize].clone(),
                (0, index),
                &f,
                &ret_tx,
                &live,
            )
        });
        let (closed, delivered) = allocations(|| {
            deliver(
                &live,
                &queue_tx,
                frame,
                "native-pisp",
                Duration::from_millis(100),
            )
        });
        assert!(!closed);
        // The consumer: take the frame, read its pixels (one byte per page), drop it.
        let (hops, consumed) = allocations(|| {
            let RecvOutcome::Data(frame) = handle.recv() else {
                panic!("no frame");
            };
            for plane in frame.planes() {
                xor ^= plane.data().iter().step_by(4096).fold(0, |a, b| a ^ b);
            }
            let hops: FrameHops = frame.meta().hops;
            drop(frame);
            hops
        });
        if measured {
            lease_allocs += leased;
            path_allocs += returned + delivered + consumed;
        }
        for hop in [
            Hop::Sensor,
            Hop::Dequeued,
            Hop::IspDone,
            Hop::Queued,
            Hop::Taken,
        ] {
            assert!(hops.get(hop).is_some(), "frame {n} has no {hop:?} hop");
        }
    }
    std::hint::black_box(xor);
    println!(
        "in-process PiSP path, allocations per frame: lease {:.2}, the rest (return, deliver, take, read, release) {:.2}",
        lease_allocs as f64 / FRAMES as f64,
        path_allocs as f64 / FRAMES as f64
    );
    assert_eq!(path_allocs, 0, "the frame path allocates");
    // The lease record (the backing the frame's shares count) is the only allocation.
    assert_eq!(
        lease_allocs, FRAMES,
        "a lease allocates more than its record"
    );
    // No frame was copied on the way (each frame's own count, carried in its hops).
    let path = handle.camera_metrics().path;
    assert_eq!((path.copied, path.zero_copy), (0, FRAMES + 100));
    let names: Vec<_> = path.hops.iter().map(|h| h.to.as_str()).collect();
    assert_eq!(names, ["dequeued", "isp_done", "queued", "taken"]);
}
