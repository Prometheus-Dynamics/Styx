//! The end of a producer's ring, seen by the source. A producer that closes its ring cleanly has
//! ended its stream: every frame it published was delivered, and nothing is wrong. The source
//! counts no failure and keeps no error. A producer whose ring ends with a fault is an error, and
//! is counted as one.

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, packed_layouts};
use styx_gst::Source;
use styx_gst::{LinkState, LinkStatus};
use styx_socket::{BeginError, ENDED_FAULTED, Limits, Producer, ProducerConfig};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 32;
const PAYLOAD: usize = (WIDTH * HEIGHT * 3 / 2) as usize;

fn entry(id: u64) -> FrameEntry {
    let format = MediaFormat::with_defaults(
        FourCc::NV12,
        Resolution::new(WIDTH, HEIGHT).expect("resolution"),
    );
    let layouts = packed_layouts(FourCc::NV12, WIDTH, HEIGHT, None, None).expect("layouts");
    FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts)
}

fn publish(producer: &mut Producer, id: u64) {
    match producer.begin(entry(id)) {
        Ok(mut lease) => {
            let mut planes = lease
                .write()
                .expect("a new frame's lease is the sole owner");
            planes.bytes().fill(id as u8);
            producer.publish(lease).expect("publish");
        }
        Err(BeginError::NoFreeBuffer) => {}
        Err(err) => panic!("begin: {err}"),
    }
}

/// Runs one producer with one source. The source reads until it has `frames` frames; then `end`
/// ends the producer's ring (the producer stays open, so the source's socket stays open too), and
/// the source reads until its link says Waiting. Returns the link's status at that moment.
fn run(name: &str, frames: u64, end: impl Fn(&mut Producer)) -> LinkStatus {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind");
    let seen = Arc::new(AtomicU64::new(0));
    let ended = Arc::new(AtomicBool::new(false));
    let reader = {
        let (seen, ended) = (Arc::clone(&seen), Arc::clone(&ended));
        let path = path.clone();
        thread::spawn(move || {
            let mut source = Source::new(path);
            let deadline = Instant::now() + Duration::from_secs(20);
            while seen.load(Ordering::Acquire) < frames && Instant::now() < deadline {
                if source.next(Duration::from_millis(5)).is_some() {
                    seen.fetch_add(1, Ordering::AcqRel);
                }
            }
            ended.store(true, Ordering::Release);
            while source.status().state != LinkState::Waiting && Instant::now() < deadline {
                let _ = source.next(Duration::from_millis(5));
            }
            source.status()
        })
    };

    let (stream, _) = listener.accept().expect("accept");
    let mut producer = Producer::new(ProducerConfig {
        payload: PAYLOAD,
        buffers: 4,
        history: 8,
    })
    .expect("producer");
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
    let mut ending = false;
    while !reader.is_finished() {
        if !ending {
            publish(&mut producer, id);
            id += 1;
            if seen.load(Ordering::Acquire) >= frames {
                end(&mut producer);
                ending = true;
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    let status = reader.join().expect("reader thread");
    drop(producer);
    let _ = std::fs::remove_file(&path);
    status
}

#[test]
fn a_closed_ring_is_the_end_of_the_stream_and_not_a_failure() {
    let status = run("gst-ended-closed.sock", 5, |producer| producer.close());
    assert_eq!(status.state, LinkState::Waiting, "the stream ended");
    assert_eq!(status.failures, 0, "a clean end is no failure");
    assert_eq!(status.last_error, None, "and keeps no error");
    assert!(status.next_delay.is_some(), "the next attempt is scheduled");
    assert_eq!(status.connects, 1, "one connection, with no attempt since");
}

#[test]
fn a_faulted_ring_is_an_error_and_is_counted() {
    let status = run("gst-ended-faulted.sock", 5, |producer| {
        producer.ring().set_end(ENDED_FAULTED);
    });
    assert_eq!(status.state, LinkState::Waiting);
    assert_eq!(status.failures, 1, "a faulted end is counted");
    assert!(status.last_error.is_some(), "and its error is kept");
}
