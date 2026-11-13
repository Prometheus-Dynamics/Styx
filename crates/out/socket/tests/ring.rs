//! Proof for the shared ring over a real Unix socket: a producer in this process, readers in
//! threads or in a child process.
//!
//! - One slow reader beside a fast one: the slow one skips frames and counts them; both see the
//!   producer's bytes in place; nothing is copied (`styx_frame::copy_counters`).
//! - A reader that exits while holding frames: the producer drops frames for want of a buffer
//!   while it holds them, and gets the buffers back once the socket closes.
//! - Limits: a reader over its own `max_held` is limited; over the producer's `max_hold` it is
//!   cut off.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, FrameLease, copy_counters, packed_layouts};
use styx_socket::{BeginError, Limits, Producer, ProducerConfig, ReadError, Reader, ReaderStats};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 32;
/// One GREY frame: 64 x 32 bytes.
const PAYLOAD: usize = (WIDTH * HEIGHT) as usize;
const CHILD_SOCKET: &str = "STYX_SOCKET_TEST_CHILD_SOCKET";

fn socket_path(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("styx-socket-{}-{name}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

fn entry(id: u64) -> FrameEntry {
    let format = MediaFormat::with_defaults(
        FourCc::GREY,
        Resolution::new(WIDTH, HEIGHT).expect("resolution"),
    );
    let layouts = packed_layouts(FourCc::GREY, WIDTH, HEIGHT, None, None).expect("layouts");
    FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts)
}

/// Takes a buffer, writes `id` into every byte, and publishes it. `Ok(false)` when the producer
/// dropped the frame for want of a buffer.
fn publish(producer: &mut Producer, id: u64) -> bool {
    match producer.begin(entry(id)) {
        Ok(mut lease) => {
            let mut planes = lease
                .write()
                .expect("a new frame's lease is the sole owner");
            planes.bytes().fill(id as u8);
            producer.publish(lease).expect("publish");
            true
        }
        Err(BeginError::NoFreeBuffer) => false,
        Err(err) => panic!("begin: {err}"),
    }
}

/// Checks the frame's bytes are the ones the producer wrote for its id, read in place.
fn check_bytes(lease: &FrameLease<()>) {
    let read = lease.read().expect("the buffer is mapped for the CPU");
    let plane = read.plane(0).expect("one plane");
    assert_eq!(plane.data.len(), PAYLOAD);
    let want = lease.id() as u8;
    assert!(
        plane.data.iter().all(|byte| *byte == want),
        "frame {} bytes",
        lease.id()
    );
}

fn no_copies() -> bool {
    copy_counters().iter().all(|counter| counter.copies() == 0)
}

/// A reader thread: takes frames until the producer says it is done and the queue is empty.
/// `pace` is how long it holds each frame, which is what makes it slow.
fn spawn_reader(
    path: PathBuf,
    pace: Option<Duration>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<ReaderStats> {
    thread::spawn(move || {
        let mut reader =
            Reader::connect(UnixStream::connect(path).expect("connect")).expect("connect");
        loop {
            match reader.next_frame().expect("a reader that is not cut off") {
                Some(frame) => {
                    check_bytes(frame.lease());
                    if let Some(pace) = pace {
                        thread::sleep(pace);
                    }
                }
                None => {
                    if stop.load(Ordering::Acquire) && reader.stats().queued == 0 {
                        break;
                    }
                    let _ = reader.wait(Some(Duration::from_millis(5)));
                }
            }
        }
        reader.stats()
    })
}

#[test]
fn two_readers_one_slow_skips_and_counts_with_no_copies() {
    const FRAMES: u64 = 400;
    let path = socket_path("two-readers");
    let listener = UnixListener::bind(&path).expect("bind");
    let mut producer = Producer::new(ProducerConfig {
        payload: PAYLOAD,
        buffers: 4,
        history: 8,
    })
    .expect("producer");
    let limits = Limits {
        max_held: 2,
        max_hold: None,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let fast = spawn_reader(path.clone(), None, stop.clone());
    let slow = spawn_reader(path.clone(), Some(Duration::from_millis(20)), stop.clone());
    for _ in 0..2 {
        let (stream, _) = listener.accept().expect("accept");
        producer.accept(stream, limits).expect("handshake");
    }

    let mut published = 0;
    for id in 1..=FRAMES {
        if publish(&mut producer, id) {
            published += 1;
        }
        thread::sleep(Duration::from_millis(1));
    }
    stop.store(true, Ordering::Release);
    let fast = fast.join().expect("fast reader");
    let slow = slow.join().expect("slow reader");
    let stats = producer.stats();
    println!("producer: {stats:?}");
    println!("fast reader: {fast:?}");
    println!("slow reader: {slow:?}");
    println!(
        "copy counters: {:?}",
        copy_counters()
            .iter()
            .map(|c| (c.site(), c.copies(), c.bytes()))
            .collect::<Vec<_>>()
    );

    assert_eq!(stats.published, published);
    assert_eq!(
        stats.dropped, 0,
        "the producer never needed to drop a frame"
    );
    assert!(slow.skipped > 0, "the slow reader skipped frames");
    assert_eq!(
        slow.delivered + slow.skipped,
        FRAMES,
        "every frame is delivered or counted as skipped"
    );
    assert!(
        slow.delivered < fast.delivered,
        "the slow reader got fewer frames"
    );
    assert_eq!(fast.delivered + fast.skipped, FRAMES);
    assert!(no_copies(), "no copies on the producer or the readers");
    let _ = std::fs::remove_file(&path);
}

/// The child process: a reader that takes one frame, holds it, and exits while holding it. It
/// does nothing unless the parent started it with the socket's path in the environment.
#[test]
fn dying_reader_child() {
    let Some(path) = std::env::var_os(CHILD_SOCKET) else {
        return;
    };
    let mut reader = Reader::connect(UnixStream::connect(path).expect("connect")).expect("connect");
    let held = loop {
        if let Some(frame) = reader.next_frame().expect("not cut off") {
            break frame;
        }
        let _ = reader.wait(Some(Duration::from_millis(5)));
    };
    assert_eq!(held.lease().id(), 1);
    check_bytes(held.lease());
    println!("HELD");
    std::io::stdout().flush().expect("flush");
    // Wait for the parent to have looked at the held buffer.
    let mut word = String::new();
    std::io::stdin()
        .read_line(&mut word)
        .expect("parent's word");
    // Exit with the frame and the reader still alive: no destructor runs, so the socket and the
    // mapping close only as the kernel closes them.
    std::process::exit(0);
}

#[test]
fn a_reader_that_dies_holding_frames_gives_its_buffers_back() {
    let path = socket_path("dies");
    let listener = UnixListener::bind(&path).expect("bind");
    let mut producer = Producer::new(ProducerConfig {
        payload: PAYLOAD,
        buffers: 2,
        history: 8,
    })
    .expect("producer");
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "dying_reader_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_SOCKET, &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("reader process");
    let (stream, _) = listener.accept().expect("accept");
    producer
        .accept(stream, Limits::default())
        .expect("handshake");

    assert!(publish(&mut producer, 1), "frame 1 is published");
    let mut out = BufReader::new(child.stdout.take().expect("child stdout"));
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(
            out.read_line(&mut line).expect("child output"),
            0,
            "the reader process ended early"
        );
        // The harness writes its own line first, without a newline, so the marker may follow it.
        if line.contains("HELD") {
            break;
        }
    }

    // Frame 1 is held by the child. Frame 2 takes the other buffer; with frame 2 newest, frame 3
    // has nowhere to go and is dropped, without waiting.
    assert!(publish(&mut producer, 2), "frame 2 is published");
    let started = Instant::now();
    assert!(
        !publish(&mut producer, 3),
        "frame 3 is dropped while the child holds frame 1"
    );
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "the drop did not wait"
    );
    let before = producer.stats();
    println!("while the reader holds frame 1: {before:?}");
    assert_eq!((before.readers, before.gone, before.dropped), (1, 0, 1));

    // The child exits while holding frame 1.
    let mut stdin = child.stdin.take().expect("child stdin");
    stdin.write_all(b"die\n").expect("tell the child to exit");
    drop(stdin);
    assert!(child.wait().expect("child exit").success());

    // Its socket is closed; the next frame's begin takes its marks away and frame 3 goes in.
    assert!(
        publish(&mut producer, 3),
        "frame 3 is published once the dead reader's holds are gone"
    );
    let after = producer.stats();
    println!("after the reader died: {after:?}");
    assert_eq!((after.readers, after.gone), (0, 1));
    assert!(no_copies());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_reader_over_its_own_limit_is_limited_and_over_the_producers_hold_time_is_cut_off() {
    let path = socket_path("limits");
    let listener = UnixListener::bind(&path).expect("bind");
    let mut producer = Producer::new(ProducerConfig {
        payload: PAYLOAD,
        buffers: 4,
        history: 8,
    })
    .expect("producer");
    let limits = Limits {
        max_held: 1,
        max_hold: Some(Duration::from_millis(50)),
    };
    let connecting = thread::spawn({
        let path = path.clone();
        move || Reader::connect(UnixStream::connect(path).expect("connect")).expect("connect")
    });
    let (stream, _) = listener.accept().expect("accept");
    producer.accept(stream, limits).expect("handshake");
    let mut reader = connecting.join().expect("reader");

    assert!(publish(&mut producer, 1));
    assert!(publish(&mut producer, 2));
    let first = reader.next_frame().expect("not cut off").expect("frame 1");
    assert_eq!(first.lease().id(), 1);
    assert!(
        reader.next_frame().expect("not cut off").is_none(),
        "held at its limit, frame 2 waits"
    );
    assert_eq!(reader.stats().limited, 1);
    assert_eq!(reader.stats().held, 1);
    check_bytes(first.lease());

    // Keep frame 1 for longer than the producer allows.
    let started = Instant::now();
    while producer.stats().cut_off == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the producer never cut the reader off"
        );
        let _ = producer.begin(entry(99));
        thread::sleep(Duration::from_millis(5));
    }
    println!(
        "cut off after {:?}: {:?}",
        started.elapsed(),
        producer.stats()
    );
    assert!(producer.readers()[0].cut_off);
    assert!(matches!(reader.next_frame(), Err(ReadError::CutOff)));
    drop(first);
    assert_eq!(
        reader.stats().held,
        0,
        "dropping the frame lets go of its buffer"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn latest_passes_over_older_frames_and_counts_them() {
    let path = socket_path("latest");
    let listener = UnixListener::bind(&path).expect("bind");
    let mut producer = Producer::new(ProducerConfig {
        payload: PAYLOAD,
        buffers: 4,
        history: 8,
    })
    .expect("producer");
    let connecting = thread::spawn({
        let path = path.clone();
        move || Reader::connect(UnixStream::connect(path).expect("connect")).expect("connect")
    });
    let (stream, _) = listener.accept().expect("accept");
    producer
        .accept(stream, Limits::default())
        .expect("handshake");
    let mut reader = connecting.join().expect("reader");

    for id in 1..=3 {
        assert!(publish(&mut producer, id));
    }
    let newest = reader.latest().expect("not cut off").expect("a frame");
    assert_eq!(newest.lease().id(), 3, "the newest frame");
    check_bytes(newest.lease());
    let stats = reader.stats();
    assert_eq!((stats.superseded, stats.queued, stats.held), (2, 0, 1));
    assert!(
        reader.latest().expect("not cut off").is_none(),
        "nothing newer"
    );
    drop(newest);
    assert_eq!(reader.stats().held, 0);
    let _ = std::fs::remove_file(&path);
}
