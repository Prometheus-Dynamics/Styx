//! A camera that stops running and comes back, seen by its readers: a stall keeps every reader
//! attached, a reader told of it gets `Stalled` once and `Resumed` once before the next frame, and
//! a plain frame loop never sees a stall at all.

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, packed_layouts};
use styx_socket::{Carried, Limits, Next, Producer, ProducerConfig, ProducerState, Reader};

const WIDTH: u32 = 16;
const HEIGHT: u32 = 16;
const PAYLOAD: usize = (WIDTH * HEIGHT) as usize;
/// The answer to "promptly": far under the doorbell's own bound.
const PROMPT: Duration = Duration::from_secs(2);

fn producer() -> Producer<()> {
    Producer::new(ProducerConfig {
        payload: PAYLOAD,
        buffers: 4,
        history: 8,
    })
    .expect("producer")
}

fn entry(id: u64) -> FrameEntry {
    let format = MediaFormat::with_defaults(
        FourCc::GREY,
        Resolution::new(WIDTH, HEIGHT).expect("resolution"),
    );
    let layouts = packed_layouts(FourCc::GREY, WIDTH, HEIGHT, None, None).expect("layouts");
    FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts)
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

fn attach(producer: &mut Producer<()>, name: &str) -> Reader {
    let path: PathBuf = std::env::temp_dir().join(format!(
        "styx-socket-stall-{}-{name}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind");
    let connect = path.clone();
    let reader = thread::spawn(move || {
        let stream = std::os::unix::net::UnixStream::connect(connect).expect("connect");
        Reader::connect(stream).expect("handshake")
    });
    let (stream, _) = listener.accept().expect("accept");
    producer
        .accept(stream, Limits::default())
        .expect("accept the reader");
    reader.join().expect("reader thread")
}

/// What a reader saw, in order, as short words.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    Stalled(u64),
    Resumed(Carried),
    Frame(u64),
}

fn now_ns() -> u64 {
    SampleTime::now().as_nanos()
}

#[test]
fn a_blocked_reader_gets_stalled_once_then_resumed_before_the_next_frame() {
    let mut producer = producer();
    let mut reader = attach(&mut producer, "blocked");
    let since = now_ns();
    let (seen, woke) = thread::scope(|scope| {
        let sleeper = scope.spawn(|| {
            let mut seen = Vec::new();
            let began = Instant::now();
            let mut first_wake = None;
            // The reader waits on its doorbell, as a real loop does, and reads events until a frame.
            loop {
                let _ = reader.wait(Some(Duration::from_secs(5)));
                if first_wake.is_none() {
                    first_wake = Some(began.elapsed());
                }
                match reader.next_event().expect("live") {
                    Next::Frame(frame) => {
                        seen.push(Seen::Frame(frame.lease().id()));
                        break;
                    }
                    Next::Stalled(ProducerState::Retrying { since_ns }) => {
                        seen.push(Seen::Stalled(since_ns));
                    }
                    Next::Stalled(other) => panic!("not a retry: {other:?}"),
                    Next::Resumed { state } => seen.push(Seen::Resumed(state)),
                    Next::Idle => {}
                }
            }
            (seen, first_wake.unwrap_or_default())
        });
        thread::sleep(Duration::from_millis(100));
        producer.stall(since);
        thread::sleep(Duration::from_millis(200));
        producer.resume(Carried::Kept);
        publish(&mut producer, 1);
        sleeper.join().expect("the sleeper")
    });
    assert_eq!(
        seen,
        vec![
            Seen::Stalled(since),
            Seen::Resumed(Carried::Kept),
            Seen::Frame(1),
        ],
        "one stall, one resume, then the frame"
    );
    assert!(woke < PROMPT, "the stall woke the sleeping reader: {woke:?}");
}

#[test]
fn a_stall_that_begins_and_ends_between_two_calls_is_silent() {
    let mut producer = producer();
    let mut reader = attach(&mut producer, "silent");
    producer.stall(now_ns());
    producer.resume(Carried::Kept);
    assert!(matches!(reader.next_event().expect("live"), Next::Idle));
    publish(&mut producer, 1);
    assert!(matches!(
        reader.next_event().expect("live"),
        Next::Frame(frame) if frame.lease().id() == 1
    ));
}

#[test]
fn a_reader_attaching_during_a_stall_reads_retrying_at_once() {
    let mut producer = producer();
    let since = now_ns();
    producer.stall(since);
    let mut reader = attach(&mut producer, "late");
    assert_eq!(reader.producer(), ProducerState::Retrying { since_ns: since });
    let mut seen = Vec::new();
    // Its first call reports the stall it is in; the next waits, silent.
    assert!(matches!(
        reader.next_event().expect("live"),
        Next::Stalled(ProducerState::Retrying { since_ns }) if since_ns == since
    ));
    assert!(matches!(reader.next_event().expect("live"), Next::Idle));
    producer.resume(Carried::Lost);
    match reader.next_event().expect("live") {
        Next::Resumed { state } => seen.push(Seen::Resumed(state)),
        other => panic!("expected the resume, got {other:?}"),
    }
    assert_eq!(seen, vec![Seen::Resumed(Carried::Lost)]);
    assert_eq!(reader.producer(), ProducerState::Running);
}

#[test]
fn a_plain_frame_loop_sees_only_frames_through_a_stall() {
    let mut producer = producer();
    let mut reader = attach(&mut producer, "plain");
    publish(&mut producer, 1);
    // Frame 1 was published before the stall: the loop takes it first.
    let first = reader
        .next_frame()
        .expect("frames read")
        .expect("frame 1 is pending");
    assert_eq!(first.lease().id(), 1);
    drop(first);
    producer.stall(now_ns());
    // Through the stall the plain loop keeps waiting: no frame, and never an error.
    for _ in 0..3 {
        assert!(reader.next_frame().expect("no error during a stall").is_none());
    }
    producer.resume(Carried::Kept);
    publish(&mut producer, 2);
    let frame = reader
        .next_frame()
        .expect("frames read")
        .expect("frame 2 is pending");
    assert_eq!(frame.lease().id(), 2);
}

#[test]
fn a_stall_wakes_a_sleeping_reader_with_no_frame() {
    let mut producer = producer();
    let mut reader = attach(&mut producer, "wake");
    let woke = thread::scope(|scope| {
        let sleeper = scope.spawn(|| {
            let began = Instant::now();
            let _ = reader.wait(Some(Duration::from_secs(5)));
            began.elapsed()
        });
        thread::sleep(Duration::from_millis(100));
        producer.stall(now_ns());
        sleeper.join().expect("the sleeper")
    });
    assert!(woke < PROMPT, "the stall rang the doorbell: {woke:?}");
}

#[test]
fn a_device_rebound_during_a_stall_is_resumed_as_lost() {
    let mut producer = producer();
    let mut reader = attach(&mut producer, "lost");
    producer.stall(now_ns());
    assert!(matches!(
        reader.next_event().expect("live"),
        Next::Stalled(_)
    ));
    producer.resume(Carried::Lost);
    assert!(matches!(
        reader.next_event().expect("live"),
        Next::Resumed {
            state: Carried::Lost
        }
    ));
}

#[test]
fn stalling_twice_keeps_the_first_time_and_resuming_twice_is_silent() {
    let mut producer = producer();
    let mut reader = attach(&mut producer, "twice");
    let first = now_ns();
    producer.stall(first);
    producer.stall(first + 1_000);
    assert_eq!(producer.producer_state(), ProducerState::Retrying { since_ns: first });
    assert!(matches!(
        reader.next_event().expect("live"),
        Next::Stalled(ProducerState::Retrying { since_ns }) if since_ns == first
    ));
    producer.resume(Carried::Kept);
    producer.resume(Carried::Lost);
    assert_eq!(producer.producer_state(), ProducerState::Running);
    assert!(matches!(
        reader.next_event().expect("live"),
        Next::Resumed {
            state: Carried::Kept
        }
    ));
    assert!(matches!(reader.next_event().expect("live"), Next::Idle));
}
