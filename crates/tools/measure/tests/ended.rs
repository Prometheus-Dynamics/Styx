//! The measure session on a ring that has ended: the run stops with the reason, not a silent wait.
//! `styx-measure` prints the reason and exits non-zero: its `read` command returns that text.

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, packed_layouts};
use styx_measure::session::{Session, SessionError};
use styx_socket::{
    ENDED_DEVICE_FAILED, Limits, Producer, ProducerConfig, ReadError, Start, StreamEnd,
};

const WIDTH: u32 = 16;
const HEIGHT: u32 = 16;
const PAYLOAD: usize = (WIDTH * HEIGHT) as usize;

fn entry(id: u64) -> FrameEntry {
    let format = MediaFormat::with_defaults(
        FourCc::GREY,
        Resolution::new(WIDTH, HEIGHT).expect("resolution"),
    );
    let layouts = packed_layouts(FourCc::GREY, WIDTH, HEIGHT, None, None).expect("layouts");
    FrameEntry::new(id, 0, format, layouts)
}

/// A producer with one session attached over the socket. The session connects in a thread.
fn session(name: &str) -> (Producer<()>, Session) {
    let path: PathBuf = std::env::temp_dir().join(format!(
        "styx-measure-ended-{}-{name}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind");
    let mut producer = Producer::new(ProducerConfig {
        payload: PAYLOAD,
        buffers: 4,
        history: 8,
    })
    .expect("producer");
    let connect = path.clone();
    let connected = thread::spawn(move || {
        Session::connect_with(&connect, None, Start::Newest).expect("the session connects")
    });
    let (stream, _) = listener.accept().expect("accept");
    producer
        .accept(stream, Limits::default())
        .expect("accept the session");
    (producer, connected.join().expect("connect thread"))
}

fn publish(producer: &mut Producer<()>, id: u64) {
    let mut lease = producer.begin(entry(id)).expect("a buffer is free");
    lease
        .write()
        .expect("a new frame's lease is the sole owner")
        .bytes()
        .fill(id as u8);
    producer.publish(lease).expect("publish");
}

#[test]
fn a_closed_ring_ends_the_run_with_the_closed_reason_promptly() {
    let (mut producer, mut session) = session("closed");
    publish(&mut producer, 1);
    publish(&mut producer, 2);
    producer.close();
    let began = Instant::now();
    let err = session
        .run(None, |stats| stats.delivered >= 100)
        .expect_err("the run ends at the close");
    assert!(began.elapsed() < Duration::from_secs(1), "ended at once");
    assert!(
        matches!(err, SessionError::Read(ReadError::Ended(StreamEnd::Closed))),
        "{err}"
    );
    let text = err.to_string();
    assert!(text.contains("closed it"), "{text}");
}

#[test]
fn a_device_failed_ring_ends_the_run_with_its_own_sentence() {
    let (mut producer, mut session) = session("faulted");
    publish(&mut producer, 1);
    producer.ring().set_end(ENDED_DEVICE_FAILED);
    let err = session
        .run(None, |stats| stats.delivered >= 100)
        .expect_err("the run ends at the fault");
    assert!(
        matches!(
            err,
            SessionError::Read(ReadError::Ended(StreamEnd::DeviceFailed))
        ),
        "{err}"
    );
    let text = err.to_string();
    assert!(text.contains("device failed for good"), "{text}");
}
