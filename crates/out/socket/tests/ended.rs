//! A ring that has ended: its publisher closed it cleanly (`Producer::close`, or the producer's
//! drop) or ended it with a fault. A reader gets the frames published before the end, then one
//! typed error from every read after, promptly, and a reader asleep on its doorbell wakes.

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, packed_layouts};
use styx_socket::{Limits, Producer, ProducerConfig, ReadError, Reader, StreamEnd};

const WIDTH: u32 = 16;
const HEIGHT: u32 = 16;
const PAYLOAD: usize = (WIDTH * HEIGHT) as usize;
/// The answer to "promptly": far under a second.
const PROMPT: Duration = Duration::from_secs(1);

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

/// Attaches one reader over a real socket: the reader connects in a thread and the producer
/// accepts it here.
fn attach(producer: &mut Producer<()>, name: &str) -> Reader {
    let path: PathBuf = std::env::temp_dir().join(format!(
        "styx-socket-ended-{}-{name}.sock",
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

#[test]
fn a_closed_ring_gives_its_frames_then_a_closed_error_promptly() {
    let mut producer = producer();
    let mut reader = attach(&mut producer, "closed");
    for id in 1..=3 {
        publish(&mut producer, id);
    }
    producer.close();
    let closed_at = Instant::now();

    for id in 1..=3 {
        let frame = reader
            .next_frame()
            .expect("the frames before the end are read")
            .expect("published before the end");
        assert_eq!(frame.lease().id(), id);
    }
    assert_eq!(reader.next_frame().err(), Some(ReadError::Ended(StreamEnd::Closed)));
    assert!(closed_at.elapsed() < PROMPT, "the end is read at once");
    // Sticky: every later read says so, and never an empty read.
    assert_eq!(reader.next_frame().err(), Some(ReadError::Ended(StreamEnd::Closed)));
}

#[test]
fn a_reader_asleep_on_its_doorbell_wakes_when_the_ring_closes() {
    let mut producer = producer();
    let mut reader = attach(&mut producer, "wake");
    let (woke, read) = thread::scope(|scope| {
        let sleeper = scope.spawn(|| {
            let began = Instant::now();
            // Five seconds is the bound; the close must wake it well before that.
            let _ = reader.wait(Some(Duration::from_secs(5)));
            (began.elapsed(), reader.next_frame())
        });
        thread::sleep(Duration::from_millis(100));
        producer.close();
        sleeper.join().expect("the sleeper")
    });
    assert!(
        woke < PROMPT,
        "the doorbell wait ends at the close: {woke:?}"
    );
    assert_eq!(read.err(), Some(ReadError::Ended(StreamEnd::Closed)));
}

#[test]
fn dropping_the_producer_ends_its_ring() {
    let mut producer = producer();
    let mut reader = attach(&mut producer, "dropped");
    publish(&mut producer, 1);
    drop(producer);
    let frame = reader
        .next_frame()
        .expect("read")
        .expect("the frame before the drop");
    assert_eq!(frame.lease().id(), 1);
    assert_eq!(reader.next_frame().err(), Some(ReadError::Ended(StreamEnd::Closed)));
}

/// Each end code is its own outcome with its own sentence, and an unknown code is kept, never read
/// as running. Frames published before the end come first.
#[test]
fn each_end_code_gives_its_own_outcome_and_sentence() {
    let ends = [
        StreamEnd::DeviceFailed,
        StreamEnd::ConsumerFault,
        StreamEnd::Unknown(7),
    ];
    let mut sentences = Vec::new();
    for end in ends {
        let mut producer = producer();
        let mut reader = attach(&mut producer, &format!("end-{}", end.code()));
        publish(&mut producer, 1);
        producer.end(end);
        assert_eq!(producer.ring().end(), end.code(), "the end code is written");
        let frame = reader
            .next_frame()
            .expect("the frame before the end is read")
            .expect("published before the end");
        assert_eq!(frame.lease().id(), 1);
        drop(frame);
        let err = reader.next_frame().err().expect("the stream has ended");
        assert_eq!(err, ReadError::Ended(end));
        // Sticky: the same end on every later read.
        assert_eq!(reader.next_frame().err(), Some(ReadError::Ended(end)));
        sentences.push(err.to_string());
    }
    assert!(sentences[0].contains("device failed for good"), "{}", sentences[0]);
    assert!(sentences[1].contains("broke the rules"), "{}", sentences[1]);
    assert!(sentences[2].contains("kind unknown") && sentences[2].contains('7'), "{}", sentences[2]);
    assert!(sentences[0] != sentences[1] && sentences[1] != sentences[2] && sentences[0] != sentences[2]);
}

#[test]
fn the_first_end_is_the_one_a_reader_is_told() {
    let mut producer = producer();
    let mut reader = attach(&mut producer, "first");
    producer.end(StreamEnd::DeviceFailed);
    producer.close();
    assert_eq!(producer.ring().end(), StreamEnd::DeviceFailed.code());
    assert_eq!(
        reader.next_frame().err(),
        Some(ReadError::Ended(StreamEnd::DeviceFailed))
    );
}

#[test]
fn end_codes_map_both_ways() {
    assert_eq!(StreamEnd::from_code(0), None, "0 is a running ring");
    assert_eq!(StreamEnd::from_code(1), Some(StreamEnd::Closed));
    assert_eq!(StreamEnd::from_code(2), Some(StreamEnd::ConsumerFault));
    assert_eq!(StreamEnd::from_code(3), Some(StreamEnd::DeviceFailed));
    assert_eq!(StreamEnd::from_code(7), Some(StreamEnd::Unknown(7)));
    for end in [StreamEnd::Closed, StreamEnd::ConsumerFault, StreamEnd::DeviceFailed, StreamEnd::Unknown(7)] {
        assert_eq!(StreamEnd::from_code(end.code()), Some(end));
    }
}

#[test]
fn latest_returns_the_newest_frame_before_the_end_and_the_end_after() {
    let mut producer = producer();
    let mut reader = attach(&mut producer, "latest");
    for id in 1..=3 {
        publish(&mut producer, id);
    }
    producer.close();
    let newest = reader
        .latest()
        .expect("the newest frame is read")
        .expect("a frame before the end");
    assert_eq!(newest.lease().id(), 3);
    drop(newest);
    assert_eq!(reader.latest().err(), Some(ReadError::Ended(StreamEnd::Closed)));
}
