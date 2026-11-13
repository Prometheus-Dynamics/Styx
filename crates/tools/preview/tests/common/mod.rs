//! Helpers for the preview tests: a source ring (`styx-socket`) of NV12 frames and readers of it.

#![allow(dead_code)]

use std::os::unix::net::UnixStream;
use std::thread;

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, FrameLease, packed_layouts};
use styx_socket::{BeginError, Limits, Producer, ProducerConfig, Reader};

/// A source ring of NV12 frames of `width` by `height`, `buffers` buffers, history 16.
pub fn source(width: u32, height: u32, buffers: usize) -> Producer {
    let payload = packed_layouts(FourCc::NV12, width, height, None, None)
        .expect("layouts")
        .span()
        .expect("span");
    Producer::new(ProducerConfig {
        payload,
        buffers,
        history: 16,
    })
    .expect("source ring")
}

/// A reader of `producer`'s ring, connected over a socket pair.
pub fn connect(producer: &mut Producer) -> Reader {
    let (ours, theirs) = UnixStream::pair().expect("socket pair");
    let connecting = thread::spawn(move || Reader::connect(theirs).expect("reader handshake"));
    producer
        .accept(ours, Limits::default())
        .expect("producer handshake");
    connecting.join().expect("reader thread")
}

/// Publishes one NV12 frame stamped `timestamp_ns`, filled with `id`. `false` when the ring had
/// no free buffer (the frame is dropped and counted by the producer).
pub fn publish(
    producer: &mut Producer,
    width: u32,
    height: u32,
    id: u64,
    timestamp_ns: u64,
) -> bool {
    let format = MediaFormat::with_defaults(
        FourCc::NV12,
        Resolution::new(width, height).expect("resolution"),
    );
    let layouts = packed_layouts(FourCc::NV12, width, height, None, None).expect("layouts");
    match producer.begin(FrameEntry::new(id, timestamp_ns, format, layouts)) {
        Ok(mut lease) => {
            {
                let mut planes = lease.write().expect("a new frame is the sole owner");
                planes.bytes().fill(id as u8);
            }
            producer.publish(lease).expect("publish");
            true
        }
        Err(BeginError::NoFreeBuffer) => false,
        Err(err) => panic!("begin: {err}"),
    }
}

/// `SampleTime::now()` as the nanoseconds the ring stamps frames with.
pub fn now_ns() -> u64 {
    SampleTime::now().as_nanos()
}

/// The first and last two bytes of a JPEG, checked as a JFIF stream starts and ends.
pub fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() > 4 && bytes[..2] == [0xFF, 0xD8] && bytes[bytes.len() - 2..] == [0xFF, 0xD9]
}

/// A lease's width and height, as its format says.
pub fn size(lease: &FrameLease<()>) -> (u32, u32) {
    let format = lease.format();
    (
        format.resolution.width.get(),
        format.resolution.height.get(),
    )
}

/// The next frame `reader` delivers, waiting up to five seconds for the producer to publish it.
pub fn next_frame(reader: &mut Reader) -> styx_socket::Frame {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(frame) = reader.next_frame().expect("reader") {
            return frame;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no frame within five seconds"
        );
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
}
