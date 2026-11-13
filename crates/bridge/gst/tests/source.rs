//! The Rust half of `styxsrc` over a real producer: frames arrive as the producer's bytes, with
//! their planes, and nothing is copied. Runs without GStreamer.

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, copy_counters, packed_layouts};
use styx_gst::Source;
use styx_socket::{BeginError, Limits, Producer, ProducerConfig};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 32;
/// NV12: a 64 x 32 luma plane, then 64 x 16 interleaved chroma.
const PAYLOAD: usize = (WIDTH * HEIGHT * 3 / 2) as usize;

fn entry(id: u64) -> FrameEntry {
    let format = MediaFormat::with_defaults(
        FourCc::NV12,
        Resolution::new(WIDTH, HEIGHT).expect("resolution"),
    );
    let layouts = packed_layouts(FourCc::NV12, WIDTH, HEIGHT, None, None).expect("layouts");
    FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts)
}

/// Writes `id` into the luma and 0x80 into the chroma, then publishes. `false` when dropped.
fn publish(producer: &mut Producer, id: u64) -> bool {
    match producer.begin(entry(id)) {
        Ok(mut lease) => {
            let mut planes = lease
                .write()
                .expect("a new frame's lease is the sole owner");
            let bytes = planes.bytes();
            let luma = (WIDTH * HEIGHT) as usize;
            bytes[..luma].fill(id as u8);
            bytes[luma..].fill(0x80);
            producer.publish(lease).expect("publish");
            true
        }
        Err(BeginError::NoFreeBuffer) => false,
        Err(err) => panic!("begin: {err}"),
    }
}

#[test]
fn frames_arrive_as_the_producers_bytes_with_their_planes_and_no_copy() {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gst-source.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind");
    let mut producer = Producer::new(ProducerConfig {
        payload: PAYLOAD,
        buffers: 4,
        history: 8,
    })
    .expect("producer");

    let reader_path = path.clone();
    let reader = thread::spawn(move || {
        let mut source = Source::new(reader_path);
        let mut ids = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while ids.len() < 30 && Instant::now() < deadline {
            let Some(frame) = source.next(Duration::from_millis(5)) else {
                continue;
            };
            assert_eq!(frame.format(), c"NV12");
            assert_eq!((frame.width(), frame.height()), (WIDTH, HEIGHT));
            let luma = frame.plane(0).expect("luma plane");
            let chroma = frame.plane(1).expect("chroma plane");
            assert_eq!((luma.offset, luma.stride), (0, WIDTH as usize));
            assert_eq!(chroma.offset, (WIDTH * HEIGHT) as usize);
            assert_eq!(chroma.stride, WIDTH as usize);
            let bytes = frame.data().expect("the memfd is mapped for the CPU");
            assert_eq!(bytes.len(), PAYLOAD);
            let id = frame.id();
            assert!(
                bytes[..(WIDTH * HEIGHT) as usize]
                    .iter()
                    .all(|byte| *byte == id as u8),
                "luma of frame {id} is the producer's"
            );
            assert!(
                bytes[(WIDTH * HEIGHT) as usize..]
                    .iter()
                    .all(|b| *b == 0x80)
            );
            assert!(
                frame.raw_fd().is_some(),
                "the buffer has a memfd to hand on"
            );
            ids.push(id);
        }
        (ids, source.stats())
    });

    let (stream, _) = listener.accept().expect("accept");
    producer
        .accept(
            stream,
            Limits {
                max_held: 2,
                max_hold: None,
            },
        )
        .expect("handshake");
    let mut id = 1_u64;
    while !reader.is_finished() {
        publish(&mut producer, id);
        id += 1;
        thread::sleep(Duration::from_millis(2));
    }
    let (ids, stats) = reader.join().expect("reader thread");
    assert!(ids.len() >= 30, "got {} frames", ids.len());
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "in publication order"
    );
    assert_eq!(stats.delivered, ids.len() as u64);
    assert_eq!(stats.unsupported + stats.unmapped, 0);
    assert!(
        copy_counters().iter().all(|counter| counter.copies() == 0),
        "no frame was copied"
    );
}

#[test]
fn a_frame_in_a_format_gstreamer_cannot_name_is_dropped_not_handed_on() {
    // MJPG has no raw GStreamer name: the source drops it and counts it.
    assert!(styx_gst::gst_format(FourCc::MJPG).is_none());
}
