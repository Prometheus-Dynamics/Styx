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
                None,
                // The default: access started on the capture thread (no allocation either).
                CpuPrep {
                    sync: true,
                    prefetch: crate::capture_api::NativeCpuPrefetch::L3,
                },
                None,
                Default::default(),
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
    // The lease record comes back too (`BackingPool`).
    assert_eq!(lease_allocs, 0, "a lease allocates");
    // No frame was copied on the way (each frame's own count, carried in its hops).
    let path = handle.camera_metrics().path;
    assert_eq!((path.copied, path.zero_copy), (0, FRAMES + 100));
    let names: Vec<_> = path.hops.iter().map(|h| h.to.as_str()).collect();
    assert_eq!(names, ["dequeued", "isp_done", "queued", "taken"]);
}

/// A companion (a pyramid level, a region, the raw frame) allocates nothing either: its lease
/// record and its place on the main frame are reused.
#[test]
fn a_pisp_companion_allocates_nothing() {
    let live = CaptureMetrics::default();
    let (ret_tx, ret_rx) = returns();
    let buffers: Vec<BeBuffer> = (0..BUFFERS).map(|_| buffer(W, H)).collect();
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
    let prep = CpuPrep {
        sync: false,
        prefetch: crate::capture_api::NativeCpuPrefetch::Off,
    };
    let (mut main_allocs, mut with_companion_allocs) = (0u64, 0u64);
    for n in 0..FRAMES + 100 {
        while let RecvOutcome::Data(_) = ret_rx.recv() {}
        f.sequence = n;
        let make = |index: u32, f: &_| {
            lease(
                placed,
                buffers[index as usize].clone(),
                (0, index),
                f,
                &ret_tx,
                &live,
                None,
                prep,
                None,
                Default::default(),
            )
        };
        let (_, main) = allocations(|| drop(make(0, &f)));
        let (_, both) = allocations(|| {
            let frame = make(0, &f)
                .with_companion(CompanionKind::Pyramid { level: 1 }, make(1, &f))
                .expect("a companion");
            drop(frame);
        });
        if n >= 100 {
            main_allocs += main;
            with_companion_allocs += both;
        }
    }
    let (main, both) = (
        main_allocs as f64 / FRAMES as f64,
        with_companion_allocs as f64 / FRAMES as f64,
    );
    assert_eq!(main, 0.0, "allocations per lease");
    assert_eq!(both, 0.0, "allocations with one companion");
}

/// The raw frame's lease (`NativeIspConfig::sensor_raw`), whole or at a sliced frame's first
/// slice with a progress of its own, allocates nothing either: its record comes from the
/// pool and its progress from the reused slice states.
#[test]
fn a_raw_lease_allocates_nothing() {
    let live = CaptureMetrics::default();
    let (ret_tx, ret_rx) = returns();
    let buffers: Vec<BeBuffer> = (0..BUFFERS).map(|_| buffer(W, H)).collect();
    // 16-bit samples: half as many per row as the buffer's bytes.
    let layout = RawLayout {
        width: W as u32 / 2,
        height: H as u32,
        stride: W as u32,
        cfa: styx_softisp::CfaPattern::Rggb,
        bits: 10,
    };
    let mut f = frame();
    let mut states = super::super::pisp_sliced::SliceStates::default();
    let mut counts = [0u64; 2];
    for n in 0..FRAMES + 100 {
        while let RecvOutcome::Data(_) = ret_rx.recv() {}
        let index = (n % BUFFERS as u64) as u32;
        f.sequence = n;
        f.raw_image = Some(index);
        for (sliced, count) in counts.iter_mut().enumerate() {
            let ((), made) = allocations(|| {
                let progress = (sliced == 1).then(|| states.take([0, 0]));
                let lease = raw_lease_of(
                    layout,
                    buffers[index as usize].clone(),
                    index,
                    &f,
                    &ret_tx,
                    &live,
                    None,
                    progress.clone(),
                )
                .expect("a raw lease");
                if let Some(p) = &progress {
                    p.progress.complete([H as u32, 0]);
                }
                assert_eq!(lease.meta().format.resolution.width.get(), W as u32 / 2);
                drop(lease);
            });
            if n >= 100 {
                *count += made;
            }
        }
    }
    assert_eq!(counts, [0, 0], "allocations of raw leases (whole, sliced)");
}
