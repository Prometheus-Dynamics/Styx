//! An MJPG frame reaches the source as `image/jpeg` of its own length: one plane, the bytes the
//! encoder wrote, with nothing after them handed to GStreamer. Needs no GStreamer.

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, Layouts, PlaneLayout};
use styx_gst::Source;
use styx_socket::{BeginError, Limits, Producer, ProducerConfig};

/// The JPEG-like frame's length: a start marker, filler, and an end marker, in a larger buffer.
const JPEG_LEN: usize = 1234;
const BUFFER: usize = 8192;

#[test]
fn an_mjpg_frame_arrives_as_image_jpeg_of_its_own_length() {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gst-mjpg.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind");
    let mut producer = Producer::new(ProducerConfig {
        payload: BUFFER,
        buffers: 4,
        history: 8,
    })
    .expect("producer");

    let reader_path = path.clone();
    let reader = thread::spawn(move || {
        let mut source = Source::new(reader_path);
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let Some(frame) = source.next(Duration::from_millis(5)) else {
                continue;
            };
            assert_eq!(frame.format(), c"image/jpeg");
            assert!(frame.compressed());
            assert_eq!(frame.payload_len(), JPEG_LEN, "the frame's own length");
            assert_eq!(frame.plane(0).expect("one plane").offset, 0);
            let bytes = frame.data().expect("mapped");
            assert_eq!(&bytes[..2], &[0xFF, 0xD8], "start marker");
            assert_eq!(&bytes[JPEG_LEN - 2..JPEG_LEN], &[0xFF, 0xD9], "end marker");
            return (frame.id(), source.stats());
        }
        panic!("no MJPG frame arrived");
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
    let format =
        MediaFormat::with_defaults(FourCc::MJPG, Resolution::new(640, 480).expect("resolution"));
    let layouts = Layouts::new(&[PlaneLayout::new(0, 0, JPEG_LEN)]).expect("layouts");
    let mut id = 1_u64;
    while !reader.is_finished() {
        let entry = FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts);
        match producer.begin(entry) {
            Ok(mut lease) => {
                let mut planes = lease.write().expect("sole owner");
                let bytes = planes.bytes();
                bytes[..2].copy_from_slice(&[0xFF, 0xD8]);
                bytes[2..JPEG_LEN - 2].fill(0x5A);
                bytes[JPEG_LEN - 2..JPEG_LEN].copy_from_slice(&[0xFF, 0xD9]);
                producer.publish(lease).expect("publish");
            }
            Err(BeginError::NoFreeBuffer) => {}
            Err(err) => panic!("begin: {err}"),
        }
        id += 1;
        thread::sleep(Duration::from_millis(3));
    }
    let (delivered_id, stats) = reader.join().expect("reader");
    assert!(delivered_id >= 1);
    assert_eq!(stats.unsupported, 0);
}
