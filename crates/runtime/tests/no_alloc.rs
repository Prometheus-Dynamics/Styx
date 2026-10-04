//! The runtime's frame path allocates nothing once streaming: the receiver's frame start
//! through the sensor service (scheduled writes), the filled buffer through the frame stream
//! (bookkeeping, the values that produced it, the lease), the frame's bytes, a 3A-style request
//! every frame, and the buffer given back on drop. 300 frames on the mock platform, counted by
//! a global allocator that counts on this test's thread only.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use common::{Mock, config, noop, sensor};
use styx_hal::mock::MockReceiver;
use styx_runtime::sync::lock;
use styx_runtime::{Camera, CameraOptions, serve_sync};
use styx_sensor::ControlRequest;

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.with(|a| a.set(a.get() + 1));
        }
        // SAFETY: the caller's contract is passed on.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract is passed on.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.with(|a| a.set(a.get() + 1));
        }
        // SAFETY: the caller's contract is passed on.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
fn the_frame_path_allocates_nothing() {
    let receiver = Arc::new(MockReceiver::new(4, 4096));
    let mut camera = Camera::<Mock>::new(Arc::clone(&receiver), sensor(), CameraOptions::default());
    let (mut frames, _) = camera.start(&config(3)).unwrap();
    let health = Arc::clone(camera.health().unwrap());
    let sensor = Arc::clone(camera.sensor());
    let mut cx = noop();
    let mut frame = |seq: u64| {
        receiver.frame(seq * 33_333_333);
        serve_sync(&*receiver, &*sensor, &health).unwrap();
        let Poll::Ready(Some(Ok(f))) = frames.poll_frame(&mut cx) else {
            panic!("frame {seq}")
        };
        assert!(f.controls.is_some());
        let sum: u32 = f.data().iter().take(64).map(|b| u32::from(*b)).sum();
        // What a 3A loop asks for after each frame's statistics.
        let exposure = Duration::from_micros(5000 + (seq % 7) * 1000);
        let landed = lock(&sensor)
            .request_at_now_landings(
                seq + 1,
                &ControlRequest {
                    exposure: Some(exposure),
                    gain: Some(1.0 + (seq % 3) as f64),
                    frame_duration: None,
                },
                Duration::from_secs(1),
            )
            .unwrap();
        assert!(!landed.is_empty());
        drop(f);
        sum
    };
    // Warm up: rings and queues reach their size.
    for seq in 0..20 {
        frame(seq);
    }
    COUNTING.with(|c| c.set(true));
    for seq in 20..320 {
        frame(seq);
    }
    COUNTING.with(|c| c.set(false));
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    assert_eq!(health.frame_syncs.get(), 320);
}
