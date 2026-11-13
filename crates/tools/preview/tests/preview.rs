//! Preview behaviour over real rings: frames in from a source ring, JPEG frames out on the
//! preview's own ring, read by a reader in another thread.

mod common;

use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use styx_preview::{Offer, Preview, PreviewConfig};
use styx_socket::{Frame, Reader};

use common::{connect, is_jpeg, next_frame, now_ns, publish, size, source};

const WIDTH: u32 = 128;
const HEIGHT: u32 = 96;
const FRAME_NS: u64 = 16_666_667;

/// Offers a frame read from a ring. The preview keeps the reader's `Frame`, so the ring's hold
/// lasts as long as the preview holds the frame.
fn offer(preview: &Preview<(), Frame>, frame: Frame) -> Offer {
    let share = frame.lease().share();
    preview.offer_held(frame, &share)
}

/// Reads JPEG frames from `reader` until `want` have arrived or `within` has passed.
fn collect(reader: &mut Reader, want: usize, within: Duration) -> Vec<(u32, u32, bool)> {
    let deadline = Instant::now() + within;
    let mut seen = Vec::new();
    while seen.len() < want && Instant::now() < deadline {
        match reader.next_frame().expect("output reader") {
            Some(frame) => {
                let lease = frame.lease();
                let (w, h) = size(lease);
                let read = lease.read().expect("readable");
                seen.push((w, h, is_jpeg(read.plane(0).expect("one plane").data)));
            }
            None => thread::sleep(Duration::from_millis(1)),
        }
    }
    seen
}

#[test]
fn sixty_fps_source_previews_at_fifteen_as_jpeg_frames() {
    let mut ring = source(WIDTH, HEIGHT, 8);
    let mut input = connect(&mut ring);
    let preview = Preview::<(), Frame>::new(PreviewConfig::new(WIDTH, HEIGHT)).expect("preview");
    let (ours, theirs) = UnixStream::pair().expect("pair");
    preview.attach(ours);
    // The worker runs the handshake between frames: the reader connects while frames flow, and
    // reads as they come, as a viewer does.
    let viewer = thread::spawn(move || {
        let mut output = Reader::connect(theirs).expect("output reader");
        collect(&mut output, 15, Duration::from_secs(10))
    });

    // Sixty frames at 60 fps by their timestamps, offered one at a time.
    let base = now_ns();
    for i in 0..60_u64 {
        assert!(publish(&mut ring, WIDTH, HEIGHT, i, base + i * FRAME_NS));
        let frame = next_frame(&mut input);
        assert_ne!(offer(&preview, frame), Offer::Refused);
        // Admitted frames are four offers apart (80 ms here): the encoder has the time to finish
        // each one before the next is queued, even on a busy machine.
        thread::sleep(Duration::from_millis(20));
    }
    // Frames 0, 4, 8, ... pass the cap: 15 of 60.
    let frames = viewer.join().expect("viewer");
    assert_eq!(frames.len(), 15, "output frames: {frames:?}");
    assert!(
        frames
            .iter()
            .all(|&(w, h, jpeg)| (w, h) == (WIDTH, HEIGHT) && jpeg)
    );
    let stats = preview.stats();
    assert_eq!(stats.offered, 60, "{stats:?}");
    assert_eq!(stats.published, 15, "{stats:?}");
    assert_eq!(stats.rate_capped, 45, "{stats:?}");
    assert_eq!(stats.refused, 0, "{stats:?}");
    assert_eq!(stats.no_buffer, 0, "{stats:?}");
    assert_eq!(stats.failed, 0, "{stats:?}");
    assert_eq!(stats.replaced, 0, "{stats:?}");
    assert_eq!(stats.readers, 1, "{stats:?}");
    assert!(stats.jpeg_bytes > 0, "{stats:?}");
}

#[test]
fn frames_larger_than_configured_are_refused() {
    let mut ring = source(WIDTH * 2, HEIGHT * 2, 4);
    let mut input = connect(&mut ring);
    let preview = Preview::<(), Frame>::new(PreviewConfig::new(WIDTH, HEIGHT)).expect("preview");
    assert!(publish(&mut ring, WIDTH * 2, HEIGHT * 2, 1, now_ns()));
    let frame = next_frame(&mut input);
    assert_eq!(offer(&preview, frame), Offer::Refused);
    assert_eq!(preview.stats().refused, 1);
}

#[test]
fn in_process_frames_offer_without_a_ring_hold() {
    let mut ring = source(WIDTH, HEIGHT, 4);
    let mut input = connect(&mut ring);
    let preview = Preview::<()>::new(PreviewConfig::new(WIDTH, HEIGHT)).expect("preview");
    assert!(publish(&mut ring, WIDTH, HEIGHT, 1, now_ns()));
    let frame = next_frame(&mut input);
    assert_eq!(preview.offer(frame.lease()), Offer::Queued);
    assert_eq!(preview.stats().offered, 1);
}

#[test]
fn bad_configuration_is_refused_at_construction() {
    assert!(Preview::<()>::new(PreviewConfig::new(WIDTH, HEIGHT).max_fps(0.0)).is_err());
    assert!(Preview::<()>::new(PreviewConfig::new(WIDTH, HEIGHT).quality(0)).is_err());
    assert!(Preview::<()>::new(PreviewConfig::new(0, HEIGHT)).is_err());
}
