//! The simulated camera with its frames stream made of dma-bufs (`memory = "dma-buf"` on the
//! board): the session maps each buffer writable, renders the scene into it in place, and reads it
//! back. Every frame is a dma-buf, renders the same image as a camera on shared memory, and
//! nothing is copied.
//!
//! Needs a dma-heap this user may allocate from (see `styx_frame::dma_case` for a host without one).

use std::time::Duration;

use styx_frame::{MemoryKind, copy_counters};
use styx_sim::memory::{Memory, dma_buf_case};
use styx_sim::{Scene, SensorTiming, SimCamera, SimFrame};

const FRAMES: usize = 30;
const WIDTH: usize = 320;
const HEIGHT: usize = 40;
/// `pRAA`: five bytes per four samples.
const ROW_BYTES: usize = WIDTH * 10 / 8;

/// The frame's rows, as the session rendered them.
fn rows(frame: &SimFrame) -> Vec<u8> {
    let read = frame.lease().read().expect("mapped for the CPU");
    let plane = read.plane(0).expect("plane 0");
    (0..HEIGHT)
        .flat_map(|y| plane.row_bytes(y, ROW_BYTES).expect("row").to_vec())
        .collect()
}

fn first_frame(cam: &mut SimCamera) -> SimFrame {
    loop {
        if let Some(frame) = cam.next(Duration::from_millis(500)).expect("next frame") {
            return frame;
        }
    }
}

#[test]
fn frames_rendered_into_dma_bufs_match_shared_memory_and_are_not_copied() {
    if !dma_buf_case("styx-sim camera frames") {
        return;
    }
    let mut shared = SimCamera::start(Scene::Pattern, 0).expect("the camera on shared memory");
    let frame = first_frame(&mut shared);
    assert_eq!(frame.lease().memory_kind(), MemoryKind::Memfd);
    let reference = rows(&frame);
    shared.release(frame);
    drop(shared);

    let mut cam = SimCamera::start_on(Scene::Pattern, 0, SensorTiming::default(), Memory::DmaBuf)
        .expect("the camera on dma-bufs");
    for _ in 0..FRAMES {
        let frame = first_frame(&mut cam);
        assert_eq!(
            frame.lease().memory_kind(),
            MemoryKind::Dmabuf,
            "frame {}: the board's stream is dma-bufs",
            frame.id()
        );
        assert_eq!(
            rows(&frame),
            reference,
            "frame {}: the image rendered into the dma-buf",
            frame.id()
        );
        cam.release(frame);
    }
    assert_eq!(cam.held_drops(), 0);
    let copies: usize = copy_counters().iter().map(|c| c.copies()).sum();
    assert_eq!(copies, 0, "frames were copied");
    println!("dma-buf frames: {FRAMES} rendered in place, each equal to shared memory, 0 copies");
}
