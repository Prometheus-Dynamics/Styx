//! The producer keeps the newest frames for readers: a reader that is keeping up gets every
//! frame, and a reader later than that window skips frames and the skips are counted exactly.
//!
//! The publisher follows the camera service's loop: a lease from the camera's pool, a publish, then
//! `reclaim`. Nothing else holds a buffer, so any frame lost is lost to the reclaim policy alone.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferMemory, BufferSize, FdMemory, FrameEntry, FramePool, packed_layouts};
use styx_socket::{Limits, Producer, Reader};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 32;
const PAYLOAD: usize = (WIDTH * HEIGHT) as usize;
/// The pool's buffers: the camera's, as the service sees them.
const BUFFERS: usize = 8;
/// The ring's entries (the service's `HISTORY`).
const HISTORY: u32 = 8;
/// The producer's keep window: the newest frames it does not give back. Within `HISTORY` and
/// `BUFFERS` minus the camera's hardware reserve (4 here).
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

fn socket_path(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "styx-socket-keep-{}-{name}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

/// A producer over `pool` with one reader connected. The reader is returned; the producer is the
/// caller's (its keep window is set by the caller).
fn connect(pool: FramePool<()>, name: &str) -> (Producer<()>, Reader) {
    let mut producer = Producer::with_pool(pool, HISTORY).expect("producer over the pool");
    let path = socket_path(name);
    let listener = UnixListener::bind(&path).expect("bind");
    let reader_path = path.clone();
    let reader = thread::spawn(move || {
        Reader::connect(UnixStream::connect(reader_path).expect("connect")).expect("handshake")
    });
    let (stream, _) = listener.accept().expect("accept");
    producer
        .accept(stream, Limits::default())
        .expect("accept the reader");
    (producer, reader.join().expect("reader thread"))
}

/// The camera's step: a lease from the pool, filled with the frame's id, published, then the
/// producer's reclaim. Panics if the camera cannot get a buffer (it must never starve here).
fn camera_frame(pool: &FramePool<()>, producer: &mut Producer<()>, id: u64) {
    let mut lease = pool
        .acquire(entry(id))
        .unwrap_or_else(|e| panic!("the camera starved at frame {id}: {e}"));
    {
        let mut planes = lease
            .write()
            .expect("a new frame's lease is the sole owner");
        planes.bytes().fill(id as u8);
    }
    producer.publish_with(lease, &[]).expect("publish");
    producer.reclaim();
}

/// Takes the frame the reader has next and checks it is `want`'s bytes. Returns its id.
fn take(reader: &mut Reader) -> Option<u64> {
    let frame = reader.next_frame().expect("a reader that is not cut off")?;
    let id = frame.lease().id();
    let read = frame.lease().read().expect("mapped for the CPU");
    let plane = read.plane(0).expect("one plane");
    assert!(
        plane.data.iter().all(|byte| *byte == id as u8),
        "frame {id} bytes are not its own"
    );
    Some(id)
}

#[test]
fn a_reader_up_to_keep_minus_one_frames_late_gets_every_frame() {
    const FRAMES: u64 = 2_000;
    // The reader takes frame `n - LAG` right after frame `n` is published: LAG is KEEP - 1 frame
    // periods, the latest a reader can be and still find every frame in the keep window.
    const LAG: u64 = KEEP as u64 - 1;
    let pool = pool();
    let (mut producer, mut reader) = connect(pool.clone(), "late");
    // A keep window of KEEP frames (the service's `keep` for this pool), set by the caller.
    producer.set_keep(KEEP);

    let mut next = 0_u64;
    for n in 0..FRAMES {
        camera_frame(&pool, &mut producer, n);
        while next + LAG <= n {
            let id = take(&mut reader).expect("frame n - LAG is published");
            assert_eq!(id, next, "frames must be contiguous");
            next += 1;
        }
    }
    while next < FRAMES {
        let id = take(&mut reader).expect("the rest is published");
        assert_eq!(id, next, "frames must be contiguous");
        next += 1;
    }
    let stats = reader.stats();
    eprintln!(
        "keep {KEEP} lag {LAG}: frames {FRAMES} delivered {} skipped {} published {} dropped {}",
        stats.delivered,
        stats.skipped,
        producer.stats().published,
        producer.stats().dropped
    );
    assert_eq!(stats.delivered, FRAMES, "every frame reached the reader");
    assert_eq!(stats.skipped, 0, "a reader that keeps up skips nothing");
    assert_eq!(producer.stats().dropped, 0, "the camera never starved");
}

#[test]
fn a_reader_later_than_the_window_skips_and_the_skips_are_counted_exactly() {
    const FRAMES: u64 = 20;
    let pool = pool();
    let (mut producer, mut reader) = connect(pool.clone(), "late-reader");
    producer.set_keep(KEEP);
    for n in 0..FRAMES {
        camera_frame(&pool, &mut producer, n);
    }
    // The reader wakes after all FRAMES: only the newest KEEP frames are still in the window.
    let mut got = Vec::new();
    while let Some(id) = take(&mut reader) {
        got.push(id);
    }
    let stats = reader.stats();
    eprintln!(
        "keep {KEEP}: {FRAMES} frames, reader woke late: delivered {:?} skipped {}",
        got, stats.skipped
    );
    let first_kept = FRAMES - KEEP as u64;
    assert_eq!(got, (first_kept..FRAMES).collect::<Vec<_>>());
    assert_eq!(stats.delivered, KEEP as u64);
    assert_eq!(
        stats.skipped, first_kept,
        "every frame before the window is one skip"
    );
    assert_eq!(producer.stats().dropped, 0, "the camera never starved");
}
