//! The end of a producer's ring, seen by the link. A producer that closes its ring cleanly has
//! ended its stream, with every frame delivered: the link counts no failure and keeps no error. A
//! producer whose ring ends with a device failure is an error, and is counted as one.

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, packed_layouts};
use styx_pipewire::{Link, LinkState, LinkStatus};
use styx_socket::{BeginError, ENDED_DEVICE_FAILED, Limits, Producer, ProducerConfig};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
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

/// A producer on `socket` that publishes until `end` is set. Then `how` ends its ring, and it stays
/// open until `done` is set, so the link's socket stays open too.
fn producer(
    socket: PathBuf,
    end: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    how: fn(&mut Producer),
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let mut producer = Producer::new(ProducerConfig {
            payload: PAYLOAD,
            buffers: 4,
            history: 8,
        })
        .expect("producer");
        let mut accepted = false;
        let mut id = 1_u64;
        let mut ended = false;
        while !done.load(Ordering::Acquire) {
            if !accepted {
                if let Ok((stream, _)) = listener.accept() {
                    stream.set_nonblocking(false).expect("blocking");
                    producer
                        .accept(
                            stream,
                            Limits {
                                max_held: 2,
                                max_hold: None,
                            },
                        )
                        .expect("handshake");
                    accepted = true;
                }
            } else if !ended {
                publish(&mut producer, id);
                id += 1;
                if end.load(Ordering::Acquire) {
                    how(&mut producer);
                    ended = true;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        drop(producer);
        drop(listener);
        let _ = std::fs::remove_file(&socket);
    })
}

/// Drives a link until `frames` frames have come, ends the producer, and drives on until the link
/// says Waiting. Returns the link's status at that moment.
fn run(name: &str, frames: u64, how: fn(&mut Producer)) -> LinkStatus {
    let socket = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let end = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let handle = producer(socket.clone(), Arc::clone(&end), Arc::clone(&done), how);
    // The producer binds on its own thread: wait for the socket, so no attempt is refused before it.
    let bound = Instant::now() + Duration::from_secs(10);
    while !socket.exists() && Instant::now() < bound {
        thread::sleep(Duration::from_millis(2));
    }
    let mut link = Link::new(&socket);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = 0;
    while seen < frames && Instant::now() < deadline {
        let now = Instant::now();
        let _ = link.drive(now);
        if link.next_frame(now).is_some() {
            seen += 1;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(seen >= frames, "frames before the end: {seen}");
    end.store(true, Ordering::Release);
    while link.status().state != LinkState::Waiting && Instant::now() < deadline {
        let now = Instant::now();
        let _ = link.drive(now);
        let _ = link.next_frame(now);
        thread::sleep(Duration::from_millis(2));
    }
    let status = link.status().clone();
    done.store(true, Ordering::Release);
    handle.join().expect("producer thread");
    let _ = std::fs::remove_file(Path::new(&socket));
    status
}

#[test]
fn a_closed_ring_is_the_end_of_the_stream_and_not_a_failure() {
    let status = run("pw-ended-closed.sock", 5, |producer| producer.close());
    assert_eq!(status.state, LinkState::Waiting, "the stream ended");
    assert_eq!(status.failures, 0, "a clean end is no failure");
    assert_eq!(status.last_error, None, "and keeps no error");
    assert!(status.next_delay.is_some(), "the next attempt is scheduled");
}

#[test]
fn a_device_failed_ring_is_an_error_and_is_counted() {
    let status = run("pw-ended-faulted.sock", 5, |producer| {
        producer.ring().set_end(ENDED_DEVICE_FAILED);
    });
    assert_eq!(status.state, LinkState::Waiting);
    assert_eq!(status.failures, 1, "a failed device end is counted");
    assert!(status.last_error.is_some(), "and its error is kept");
}
