//! Cutting off what holds a frame the camera needs: an adopter's mark, written straight into the
//! hold table, is cut off through the table; and a reader cut off while it uses a frame's bytes
//! gets `Torn` for a value read from a buffer that was reused, never the newer frame's bytes as
//! its own.
//!
//! Deterministic: the camera and the readers take turns on one thread.

use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;

use shm_ring::SampleTime;
use shm_ring_buffers::{BufferIndex, Marker};
use shm_ring_holds::Marks;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferMemory, BufferSize, FdMemory, FrameEntry, FramePool, packed_layouts};
use styx_socket::{Limits, Producer, Reader, Torn};

const WIDTH: u32 = 32;
const HEIGHT: u32 = 16;
const PAYLOAD: usize = (WIDTH * HEIGHT) as usize;
const BUFFERS: usize = 8;
const HISTORY: u32 = 8;

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

/// A producer keeping only the newest frame, with every other buffer the camera's: the bound is
/// one frame out of the camera's hands, so any frame held past the newest must be cut.
fn producer(pool: FramePool<()>) -> Producer<()> {
    let mut producer = Producer::with_pool(pool, HISTORY).expect("producer over the pool");
    assert_eq!(producer.set_keep(1), 1);
    assert_eq!(producer.set_reserve(BUFFERS - 1), BUFFERS - 1);
    producer
}

fn attach_reader(producer: &mut Producer<()>) -> Reader {
    let dir = tempfile::Builder::new()
        .prefix("styx-sock-")
        .tempdir()
        .expect("a scratch folder");
    let path = dir.path().join("cut.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let connect = path.clone();
    let reader = thread::spawn(move || {
        Reader::connect(UnixStream::connect(connect).expect("connect")).expect("handshake")
    });
    let (stream, _) = listener.accept().expect("accept");
    producer
        .accept(stream, Limits::default())
        .expect("accept the reader");
    reader.join().expect("reader thread")
}

/// The camera's step: a frame filled with its id's byte, published, then reclaim. Returns the
/// buffer it went into.
fn camera_frame(pool: &FramePool<()>, producer: &mut Producer<()>, id: u64) -> usize {
    let mut lease = pool
        .acquire(entry(id))
        .unwrap_or_else(|e| panic!("the camera starved at frame {id}: {e}"));
    let buffer = lease.buffer_index();
    lease
        .write()
        .expect("a new frame's lease is the sole owner")
        .bytes()
        .fill(id as u8);
    producer.publish_with(lease, &[]).expect("publish");
    producer.reclaim();
    buffer
}

#[test]
fn an_adopters_mark_on_a_frame_the_camera_needs_is_cut_off_through_the_hold_table() {
    let pool = pool();
    let mut producer = producer(pool.clone());
    // The adopter (Orion's subscriber, say) has a slot of its own in the producer's hold table,
    // with a marks page only it writes. The producer has no link for it.
    let slot = producer.holds().free_slot().expect("a free slot");
    let marks = Marks::create(BUFFERS as u32, slot, 0).expect("marks page");
    let page: OwnedFd = marks.page_fd().expect("the page, read-only");
    producer
        .holds_mut()
        .add(slot, page, Limits::default())
        .expect("the adopter's marks in the table");

    let first = camera_frame(&pool, &mut producer, 1);
    assert!(
        marks.mark(BufferIndex(first as u32)),
        "the adopter holds frame 1"
    );

    // Frame 2 makes frame 1 past the window, held by the adopter alone: over the bound.
    camera_frame(&pool, &mut producer, 2);
    let stats = producer.stats();
    assert_eq!(
        stats.adopter_cuts, 1,
        "the adopter's slot was cut off: {stats:?}"
    );
    assert_eq!(stats.bound_unmet, 0, "the bound was met: {stats:?}");
    assert_eq!(
        stats.outstanding, 1,
        "only the newest frame out of the camera's hands"
    );
    assert!(producer.holds().is_cut_off(slot), "the table says so");
    assert!(
        producer
            .holds()
            .holders_of(BufferIndex(first as u32))
            .is_empty(),
        "a cut-off slot holds nothing"
    );

    // The camera runs on: every buffer comes round, none starves, no second cut.
    for id in 3..3 + 2 * BUFFERS as u64 {
        camera_frame(&pool, &mut producer, id);
    }
    let stats = producer.stats();
    assert_eq!((stats.adopter_cuts, stats.bound_unmet), (1, 0), "{stats:?}");
}

#[test]
fn a_cut_reader_reading_a_reused_buffer_gets_torn_not_the_newer_frame() {
    let pool = pool();
    let mut producer = producer(pool.clone());
    let mut reader = attach_reader(&mut producer);

    let first = camera_frame(&pool, &mut producer, 1);
    let frame = reader
        .next_frame()
        .expect("a frame")
        .expect("frame 1 is published");
    assert_eq!(frame.buffer(), first);
    // Read while held: the bytes are the frame's, and the check agrees.
    let sum = frame
        .read(|lease| {
            let read = lease.read().expect("mapped for the CPU");
            read.plane(0)
                .expect("one plane")
                .data
                .iter()
                .map(|b| u64::from(*b))
                .sum::<u64>()
        })
        .expect("not reused yet");
    assert_eq!(sum, PAYLOAD as u64, "frame 1 is all ones");
    assert_eq!(frame.check(), Ok(()));

    // The camera needs frame 1's buffer: the reader holding it is cut off, the buffer goes back,
    // and the camera writes newer frames until it comes round to that buffer again.
    let mut reused = false;
    for id in 2..2 + 2 * BUFFERS as u64 {
        if camera_frame(&pool, &mut producer, id) == first {
            reused = true;
            break;
        }
    }
    assert!(reused, "the cut reader's buffer came back to the camera");
    assert_eq!(producer.stats().cut_off_for_reserve, 1);

    // The cut reader still has the frame in hand. Whatever it reads now is a newer frame's bytes:
    // dropped as torn, never returned as frame 1's.
    let torn = Torn { buffer: first };
    assert_eq!(frame.check(), Err(torn));
    let mut ran = false;
    assert_eq!(
        frame.read(|_| {
            ran = true;
        }),
        Err(torn)
    );
    assert!(ran, "the read ran; its value was dropped after the check");
    assert_eq!(reader.stats().torn, 2, "both counted");
    drop(frame);
    assert!(
        reader.next_frame().is_err(),
        "and the reader hears it was cut off"
    );
}
