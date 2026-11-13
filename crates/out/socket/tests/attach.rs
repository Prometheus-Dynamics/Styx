//! A reader's first frame at attach, by its start (`Start::Newest` and `Start::NextNew`).
//!
//! The producer publishes at a fixed period, each frame stamped with the time it was entered. A
//! reader attaches mid-stream over a real Unix socket. The handshake runs in a thread while the
//! producer waits for it, so nothing is published between the reader's start and its link.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferMemory, BufferSize, FdMemory, FrameEntry, FramePool, packed_layouts};
use styx_socket::{Limits, Producer, Reader, Start};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 32;
const PAYLOAD: usize = (WIDTH * HEIGHT) as usize;
/// The producer's frame period.
const PERIOD: Duration = Duration::from_millis(20);
/// Well under a period: a frame that arrives this fast was not waited for on a period's bell.
const FAST: Duration = Duration::from_millis(10);
const BUFFERS: usize = 8;
const HISTORY: u32 = 8;
const KEEP: usize = 4;

fn pool() -> FramePool<()> {
    let memories = (0..BUFFERS).map(|_| {
        let fd = shm_ring::sealed_memfd(PAYLOAD).expect("memfd");
        Box::new(FdMemory::memfd(fd, PAYLOAD, true).expect("map")) as Box<dyn BufferMemory>
    });
    FramePool::new(BufferSize::new(PAYLOAD, 0), memories).expect("pool")
}

fn entry(id: u64) -> FrameEntry {
    let format = MediaFormat::with_defaults(
        FourCc::GREY,
        Resolution::new(WIDTH, HEIGHT).expect("resolution"),
    );
    let layouts = packed_layouts(FourCc::GREY, WIDTH, HEIGHT, None, None).expect("layouts");
    FrameEntry::new(id, SampleTime::now().as_nanos(), format, layouts)
}

/// One frame, as the camera makes it: a lease, filled with the id, published, then reclaimed.
fn publish(pool: &FramePool<()>, producer: &mut Producer<()>, id: u64) {
    let mut lease = pool.acquire(entry(id)).expect("the camera has a buffer");
    {
        let mut planes = lease
            .write()
            .expect("a new frame's lease is the sole owner");
        planes.bytes().fill(id as u8);
    }
    producer.publish_with(lease, &[]).expect("publish");
    producer.reclaim();
}

/// Publishes `from..to`, one period apart.
fn publish_run(pool: &FramePool<()>, producer: &mut Producer<()>, from: u64, to: u64) {
    for id in from..to {
        publish(pool, producer, id);
        thread::sleep(PERIOD);
    }
}

fn socket_path(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "styx-socket-attach-{}-{name}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

/// Attaches a reader with `start`: the reader connects in a thread, the producer accepts it on
/// this one. The producer publishes nothing while the handshake runs.
fn attach(producer: &mut Producer<()>, name: &str, start: Start) -> Reader {
    let path = socket_path(name);
    let listener = UnixListener::bind(&path).expect("bind");
    let reader_path = path.clone();
    let reader = thread::spawn(move || {
        Reader::connect_with(UnixStream::connect(reader_path).expect("connect"), start)
            .expect("handshake")
    });
    let (stream, _) = listener.accept().expect("accept");
    producer
        .accept(stream, Limits::default())
        .expect("accept the reader");
    reader.join().expect("reader thread")
}

/// The next frame's id and how old it was when it arrived. The frame is checked to hold its
/// own id in every byte.
fn take_id(reader: &mut Reader) -> Option<(u64, Duration)> {
    let frame = reader.next_frame().expect("a reader that is not cut off")?;
    let id = frame.lease().id();
    let read = frame.lease().read().expect("mapped for the CPU");
    let plane = read.plane(0).expect("one plane");
    assert!(
        plane.data.iter().all(|byte| *byte == id as u8),
        "frame {id} bytes are not its own"
    );
    Some((id, Duration::from_nanos(frame.age_ns())))
}

#[test]
fn newest_start_delivers_the_newest_kept_frame_at_once_then_the_rest_in_order() {
    let pool = pool();
    let mut producer: Producer<()> = Producer::with_pool(pool.clone(), HISTORY).expect("producer");
    producer.set_keep(KEEP);
    // Twenty frames are published before the reader attaches: the newest is 19.
    publish_run(&pool, &mut producer, 0, 20);
    let mut reader = attach(&mut producer, "newest", Start::Newest);

    // The newest kept frame is there at once: no bell is waited for.
    let began = Instant::now();
    let (first, _) = take_id(&mut reader).expect("the newest kept frame is delivered at attach");
    let waited = began.elapsed();
    assert_eq!(first, 19, "the first frame is the newest published one");
    assert!(
        waited < FAST,
        "the first frame took {waited:?}, not at once (a period is {PERIOD:?})"
    );

    // Then every frame published after it, in order, with no duplicate and nothing skipped.
    let mut ids = vec![first];
    for id in 20..40 {
        publish(&pool, &mut producer, id);
        reader.wait(Some(PERIOD * 5)).expect("wait");
        while let Some((got, _)) = take_id(&mut reader) {
            ids.push(got);
        }
        thread::sleep(PERIOD);
    }
    while let Some((got, _)) = take_id(&mut reader) {
        ids.push(got);
    }
    assert_eq!(
        ids,
        (19..40).collect::<Vec<_>>(),
        "consecutive, no duplicate"
    );
    let stats = reader.stats();
    assert_eq!(stats.skipped, 0, "a reader that keeps up skips nothing");
    assert_eq!(stats.delivered, 21);
    eprintln!("newest: first frame in {waited:?}; ids {ids:?}");
}

#[test]
fn next_new_start_delivers_only_frames_published_after_the_attach() {
    let pool = pool();
    let mut producer: Producer<()> = Producer::with_pool(pool.clone(), HISTORY).expect("producer");
    producer.set_keep(KEEP);
    publish_run(&pool, &mut producer, 0, 20);
    // The newest published frame at attach is 19.
    let mut reader = attach(&mut producer, "next-new", Start::NextNew);
    assert!(
        reader.next_frame().expect("not cut off").is_none(),
        "the frames already published are skipped"
    );

    let mut first: Option<(u64, Duration)> = None;
    for id in 20..40 {
        publish(&pool, &mut producer, id);
        reader.wait(Some(PERIOD * 5)).expect("wait");
        while let Some((got, age)) = take_id(&mut reader) {
            assert!(got > 19, "frame {got} was published before the attach");
            if first.is_none() {
                first = Some((got, age));
            }
        }
        thread::sleep(PERIOD);
    }
    let (id, age) = first.expect("a frame arrived");
    eprintln!("next-new: first id {id}, arrival minus publish {age:?}");
    assert_eq!(id, 20, "the first frame is the first one after the attach");
    assert!(
        age < FAST,
        "the first frame arrived {age:?} after its publish (a period is {PERIOD:?})"
    );
}

#[test]
fn an_attach_with_nothing_published_waits_for_the_first_frame() {
    let pool = pool();
    let mut producer: Producer<()> = Producer::with_pool(pool.clone(), HISTORY).expect("producer");
    producer.set_keep(KEEP);
    let mut reader = attach(&mut producer, "empty", Start::Newest);
    assert!(
        reader.next_frame().expect("not cut off").is_none(),
        "nothing is published, so nothing is delivered"
    );

    // The reader sleeps on its bell until the first frame is published, a while later.
    let (published_at, woke_at) = thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            reader.wait(Some(Duration::from_secs(5))).expect("wait");
            Instant::now()
        });
        thread::sleep(PERIOD * 2);
        let published_at = Instant::now();
        publish(&pool, &mut producer, 0);
        (published_at, waiter.join().expect("waiter"))
    });
    assert!(
        woke_at >= published_at,
        "the reader woke before the frame was published"
    );
    let (id, _) = take_id(&mut reader).expect("the first frame");
    assert_eq!(id, 0);
}
