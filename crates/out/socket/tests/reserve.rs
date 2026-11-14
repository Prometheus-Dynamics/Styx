//! The camera's reserve: the buffers out of the camera's hands (the frames in the keep window, plus
//! the frames readers hold past it) never exceed `buffers - reserve`. A reader that holds a frame
//! past the keep window while the bound needs it is cut off; a reader inside its rights is not.
//!
//! Everything here is deterministic: the publisher and the readers take turns on one thread, and
//! no assertion depends on time.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferMemory, BufferSize, FdMemory, FrameEntry, FramePool, packed_layouts};
use styx_socket::{Frame, Limits, Producer, ReadError, Reader, StreamEnd};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 32;
const PAYLOAD: usize = (WIDTH * HEIGHT) as usize;
/// The camera's pool, as the service sees it.
const BUFFERS: usize = 8;
const HISTORY: u32 = 8;
/// Frames published, one per step.
const FRAMES: u64 = 40;

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

/// A producer over `pool` with the given keep window and reserve, and no readers yet.
fn producer(pool: FramePool<()>, keep: usize, reserve: usize) -> Producer<()> {
    let mut producer = Producer::with_pool(pool, HISTORY).expect("producer over the pool");
    assert_eq!(producer.set_keep(keep), keep, "the keep window in force");
    assert_eq!(
        producer.set_reserve(reserve),
        reserve,
        "the reserve in force"
    );
    producer
}

/// Connects one reader to `producer`, over a socket of its own. Tests run in parallel, so the
/// name carries a counter as well as the process id: two tests never share a path.
fn attach_reader(producer: &mut Producer<()>, name: &str) -> Reader {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path: PathBuf = std::env::temp_dir().join(format!(
        "styx-socket-reserve-{}-{}-{name}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind");
    let connect = path.clone();
    let reader = thread::spawn(move || {
        Reader::connect(UnixStream::connect(connect).expect("connect")).expect("handshake")
    });
    let (stream, _) = listener.accept().expect("accept");
    producer
        .accept(stream, Limits::default())
        .expect("accept the reader");
    let reader = reader.join().expect("reader thread");
    let _ = std::fs::remove_file(&path);
    reader
}

/// The camera's step: a lease from the pool, filled with the frame's id, published, then reclaim.
/// Panics if the camera cannot get a buffer.
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

/// Whether the camera's pool can still give out `want` buffers at once. Takes them and gives them
/// back, so it is a check, not a step.
fn camera_has(pool: &FramePool<()>, want: usize) -> bool {
    let mut leases = Vec::with_capacity(want);
    for k in 0..want {
        match pool.acquire(entry(100_000 + k as u64)) {
            Ok(lease) => leases.push(lease),
            Err(_) => return false,
        }
    }
    true
}

/// The reader's next frame, checked to be the bytes of its id. `Ok(None)` when nothing is new.
fn take(reader: &mut Reader) -> Result<Option<Frame>, ReadError> {
    let frame = reader.next_frame()?;
    if let Some(frame) = &frame {
        let id = frame.lease().id();
        let read = frame.lease().read().expect("mapped for the CPU");
        let plane = read.plane(0).expect("one plane");
        assert!(
            plane.data.iter().all(|byte| *byte == id as u8),
            "frame {id} bytes are not its own"
        );
    }
    Ok(frame)
}

/// The invariant, after any step: out of the camera's hands at most `buffers - reserve`, and the
/// camera can still take `reserve` buffers at once.
fn assert_bound(producer: &Producer<()>, pool: &FramePool<()>, reserve: usize, at: u64) {
    let stats = producer.stats();
    assert!(
        stats.outstanding <= BUFFERS - reserve,
        "after frame {at}: {} buffers out of the camera's hands, the bound is {}",
        stats.outstanding,
        BUFFERS - reserve
    );
    assert!(
        camera_has(pool, reserve),
        "after frame {at}: the camera cannot take its reserve of {reserve} buffers"
    );
}

#[test]
fn a_reader_holding_the_four_oldest_frames_is_cut_off_when_its_frame_leaves_the_window() {
    const KEEP: usize = 4;
    const RESERVE: usize = 4;
    let pool = pool();
    let mut producer = producer(pool.clone(), KEEP, RESERVE);
    let mut holder = attach_reader(&mut producer, "holder");

    // Frames 0..4 are published; the holder reads all four and keeps them.
    let mut held: Vec<Frame> = Vec::new();
    for id in 0..KEEP as u64 {
        camera_frame(&pool, &mut producer, id);
        let frame = take(&mut holder)
            .expect("not cut off yet")
            .expect("the frame is published");
        assert_eq!(frame.lease().id(), id);
        held.push(frame);
    }
    assert_eq!(producer.stats().cut_off_for_reserve, 0);

    for id in KEEP as u64..FRAMES {
        // Before frame 4 leaves the window nothing is cut off; the publish of frame 4 is the one
        // that takes frame 0 out of the window while the holder still holds it.
        if id == KEEP as u64 {
            assert_eq!(producer.stats().cut_off_for_reserve, 0);
            assert!(!producer.readers()[0].cut_off);
        }
        camera_frame(&pool, &mut producer, id);
        assert_bound(&producer, &pool, RESERVE, id);
        if id == KEEP as u64 {
            assert_eq!(
                producer.stats().cut_off_for_reserve,
                1,
                "the holder is cut off on the publish that takes its oldest frame out of the window"
            );
            assert!(producer.readers()[0].cut_off && producer.readers()[0].for_reserve);
        }
    }
    let stats = producer.stats();
    assert_eq!(stats.cut_off, 1, "only the holder is cut off");
    assert_eq!(stats.cut_off_for_reserve, 1);
    assert_eq!(stats.bound_unmet, 0, "every publish kept the reserve");
    assert_eq!(stats.dropped, 0, "the camera never starved");

    // The cut-off reader reads the end, with the reason, and takes no more frames.
    let end = take(&mut holder).expect_err("the holder is cut off");
    assert_eq!(end, ReadError::Ended(StreamEnd::ConsumerFault));
    assert!(
        end.to_string().contains("past the keep window"),
        "the message names the cause: {end}"
    );
    // The frames it held are released; it must not read them now.
    drop(held);
}

#[test]
fn a_reader_that_reads_and_releases_is_never_cut_off_and_misses_no_frame() {
    const KEEP: usize = 4;
    const RESERVE: usize = 4;
    let pool = pool();
    let mut producer = producer(pool.clone(), KEEP, RESERVE);
    let mut holder = attach_reader(&mut producer, "holder");
    let mut good = attach_reader(&mut producer, "good");

    let mut good_frames = Vec::new();
    let mut held: Vec<Frame> = Vec::new();
    for id in 0..FRAMES {
        camera_frame(&pool, &mut producer, id);
        assert_bound(&producer, &pool, RESERVE, id);
        // The holder keeps its first four frames; the good reader takes each frame and lets go.
        if id < KEEP as u64 {
            let frame = take(&mut holder)
                .expect("not cut off yet")
                .expect("the frame is published");
            assert_eq!(frame.lease().id(), id);
            held.push(frame);
        }
        let frame = take(&mut good)
            .expect("the good reader is never cut off")
            .expect("the frame is published");
        good_frames.push(frame.lease().id());
        drop(frame);
    }
    // The holder is cut off when frame 0 leaves the window; the good reader saw every frame.
    drop(held);
    assert_eq!(good_frames, (0..FRAMES).collect::<Vec<_>>());
    let stats = good.stats();
    assert_eq!(
        stats.delivered, FRAMES,
        "every frame reached the good reader"
    );
    assert_eq!(stats.skipped, 0, "a reader that keeps up skips nothing");
    assert_eq!(producer.stats().cut_off_for_reserve, 1, "only the holder");
    let readers = producer.readers();
    assert_eq!(readers.iter().filter(|r| r.cut_off).count(), 1);
    assert_eq!(
        take(&mut holder).expect_err("the holder is cut off"),
        ReadError::Ended(StreamEnd::ConsumerFault)
    );
    // The good reader is still served after the cut.
    camera_frame(&pool, &mut producer, FRAMES);
    assert_eq!(
        take(&mut good)
            .expect("not cut off")
            .map(|f| f.lease().id()),
        Some(FRAMES)
    );
}

#[test]
fn a_reader_holding_one_frame_inside_the_window_is_never_cut_off() {
    const KEEP: usize = 4;
    const RESERVE: usize = 4;
    let pool = pool();
    let mut producer = producer(pool.clone(), KEEP, RESERVE);
    let mut reader = attach_reader(&mut producer, "inside");

    // Frame 5 is held from its publish (age 0) through the publish of frame 8 (age 3): always
    // inside the window of 4. It is let go before frame 9 takes it out of the window.
    const HELD: u64 = 5;
    let mut held: Option<Frame> = None;
    let mut delivered = 0_u64;
    for id in 0..FRAMES {
        if id == HELD + KEEP as u64 {
            held = None;
        }
        camera_frame(&pool, &mut producer, id);
        assert_bound(&producer, &pool, RESERVE, id);
        while let Some(frame) = take(&mut reader).expect("never cut off") {
            delivered += 1;
            if frame.lease().id() == HELD {
                held = Some(frame);
            }
        }
    }
    assert!(held.is_none(), "the hold ended before frame 9");
    assert_eq!(delivered, FRAMES);
    let stats = producer.stats();
    assert_eq!(
        stats.cut_off, 0,
        "a hold inside the window costs the camera nothing"
    );
    assert_eq!(stats.cut_off_for_reserve, 0);
    assert_eq!(stats.bound_unmet, 0);
    assert_eq!(reader.stats().skipped, 0);
}

#[test]
fn with_reserve_two_a_reader_may_hold_two_frames_past_the_window() {
    const KEEP: usize = 4;
    const RESERVE: usize = 2;
    let pool = pool();
    let mut producer = producer(pool.clone(), KEEP, RESERVE);
    let mut reader = attach_reader(&mut producer, "two");

    // The reader keeps frames 0 and 1 for the whole run; it reads nothing after them.
    let mut held: Vec<Frame> = Vec::new();
    let mut peak = 0;
    for id in 0..FRAMES {
        camera_frame(&pool, &mut producer, id);
        if id < 2 {
            held.push(take(&mut reader).expect("not cut off").expect("published"));
        }
        assert_bound(&producer, &pool, RESERVE, id);
        peak = peak.max(producer.stats().outstanding);
    }
    assert_eq!(
        peak,
        BUFFERS - RESERVE,
        "the bound is used: four in the window and two held"
    );
    let stats = producer.stats();
    assert_eq!(
        stats.cut_off, 0,
        "two frames past the window are inside the reserve"
    );
    assert_eq!(stats.cut_off_for_reserve, 0);
    assert_eq!(stats.bound_unmet, 0);
    drop(held);
}

#[test]
fn with_reserve_two_the_third_frame_past_the_window_cuts_its_holder_off() {
    const KEEP: usize = 4;
    const RESERVE: usize = 2;
    let pool = pool();
    let mut producer = producer(pool.clone(), KEEP, RESERVE);
    let mut reader = attach_reader(&mut producer, "three");

    // The reader holds frames 0, 1 and 2. Frame 0 leaves the window at publish 4, frame 1 at 5,
    // frame 2 at 6: the third frame past the window makes six plus one, over the bound of six.
    let mut held: Vec<Frame> = Vec::new();
    for id in 0..=6_u64 {
        camera_frame(&pool, &mut producer, id);
        if id < 3 {
            held.push(take(&mut reader).expect("not cut off").expect("published"));
        }
        assert_bound(&producer, &pool, RESERVE, id);
        let cut = producer.stats().cut_off_for_reserve;
        assert_eq!(
            cut,
            u64::from(id == 6),
            "cut off on the publish of frame 6, and not before (frame {id})"
        );
    }
    assert_eq!(producer.stats().bound_unmet, 0);
    assert_eq!(
        take(&mut reader).expect_err("cut off"),
        ReadError::Ended(StreamEnd::ConsumerFault)
    );
    drop(held);
}

#[test]
fn the_keep_window_and_the_reserve_never_add_up_to_more_than_the_pool() {
    let mut producer = Producer::with_pool(pool(), HISTORY).expect("producer");
    assert_eq!(producer.reserve(), 0, "the default reserve is zero");
    assert_eq!(
        producer.set_reserve(BUFFERS + 3),
        BUFFERS - 1,
        "one frame is always kept"
    );
    assert_eq!(
        producer.set_keep(BUFFERS),
        1,
        "keep gives way to the reserve"
    );
    assert_eq!(producer.set_reserve(0), 0);
    assert_eq!(producer.set_keep(BUFFERS), BUFFERS.min(HISTORY as usize));
    assert_eq!(
        producer.set_reserve(4),
        0,
        "no reserve fits beside a full window"
    );
    assert_eq!(producer.set_keep(4), 4);
    assert_eq!(producer.set_reserve(4), 4);
    assert!(producer.keep() + producer.reserve() <= BUFFERS);
}
