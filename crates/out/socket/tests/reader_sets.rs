//! The reader sets a pool fits, for the reserves a held camera stream gives the outlet. A camera
//! the consumer holds `hold` frames of gets `reserve = max(max(B / 2, 2), B - hold + 1)` with
//! `hold = B - max(B / 2, 2)`; the bound is then `B - reserve` frames out of the camera's hands,
//! the newest among them, so all readers together may keep `B - reserve - 1` frames across a
//! publish. Past that the reader holding the oldest frame is cut off with a consumer fault.
//!
//! Deterministic: the camera and the readers take turns on one thread.

use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;

use shm_ring::SampleTime;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferMemory, BufferSize, FdMemory, FrameEntry, FramePool, packed_layouts};
use styx_socket::{Frame, Limits, Producer, ReadError, Reader, StreamEnd};

const WIDTH: u32 = 32;
const HEIGHT: u32 = 16;
const PAYLOAD: usize = (WIDTH * HEIGHT) as usize;
const HISTORY: u32 = 8;
const FRAMES: u64 = 24;

/// One row of the table: the pool, the consumer hold Lemnos declares, the outlet's reserve, its
/// keep window, and how many frames all readers together may keep across a publish.
struct Row {
    buffers: usize,
    hold: usize,
    reserve: usize,
    keep: usize,
    readers_may_keep: usize,
}

const TABLE: [Row; 4] = [
    Row {
        buffers: 4,
        hold: 2,
        reserve: 3,
        keep: 1,
        readers_may_keep: 0,
    },
    Row {
        buffers: 6,
        hold: 3,
        reserve: 4,
        keep: 2,
        readers_may_keep: 1,
    },
    Row {
        buffers: 8,
        hold: 4,
        reserve: 5,
        keep: 3,
        readers_may_keep: 2,
    },
    Row {
        buffers: 9,
        hold: 5,
        reserve: 5,
        keep: 4,
        readers_may_keep: 3,
    },
];

fn pool(buffers: usize) -> FramePool<()> {
    let memories = (0..buffers).map(|_| {
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

fn producer(pool: FramePool<()>, row: &Row) -> Producer<()> {
    let mut producer = Producer::with_pool(pool, HISTORY).expect("producer over the pool");
    let keep = (HISTORY as usize).min(row.buffers - row.reserve).max(1);
    assert_eq!(keep, row.keep, "B = {}: the keep window", row.buffers);
    assert_eq!(producer.set_keep(keep), keep);
    assert_eq!(producer.set_reserve(row.reserve), row.reserve);
    producer
}

fn attach(producer: &mut Producer<()>, name: &str) -> Reader {
    let dir = tempfile::Builder::new()
        .prefix("styx-sock-")
        .tempdir()
        .expect("a scratch folder");
    let path = dir.path().join(format!("{name}.sock"));
    let listener = UnixListener::bind(&path).expect("bind");
    let reader = thread::spawn(move || {
        Reader::connect(UnixStream::connect(path).expect("connect")).expect("handshake")
    });
    let (stream, _) = listener.accept().expect("accept");
    producer
        .accept(stream, Limits::default())
        .expect("accept the reader");
    reader.join().expect("reader thread")
}

fn publish(pool: &FramePool<()>, producer: &mut Producer<()>, id: u64) {
    let lease = pool
        .acquire(entry(id))
        .unwrap_or_else(|e| panic!("the camera starved at frame {id}: {e}"));
    producer.publish_with(lease, &[]).expect("publish");
    producer.reclaim();
}

/// A reader that keeps its newest `depth` frames: on each publish it reads the new frame and
/// lets go of the oldest beyond `depth`. Between publishes it holds `depth` frames, all older than
/// the next one. `ended` is how its stream ended, `None` while it runs.
struct Keeper {
    reader: Reader,
    depth: usize,
    held: Vec<Frame>,
    ended: Option<ReadError>,
    /// Reads at most this many frames, then keeps what it holds without reading on.
    reads: u64,
}

impl Keeper {
    fn step(&mut self) {
        if self.ended.is_some() || self.reads == 0 {
            return;
        }
        self.reads -= 1;
        match self.reader.next_frame() {
            Ok(Some(frame)) => {
                self.held.push(frame);
                if self.held.len() > self.depth {
                    self.held.remove(0);
                }
            }
            Ok(None) => {}
            Err(e) => {
                self.held.clear();
                self.ended = Some(e);
            }
        }
    }
}

/// Runs readers that keep the given depths through a stream on `row`'s pool. Returns, per reader,
/// whether it was cut off for the reserve, and the producer's cut count.
fn run(row: &Row, depths: &[usize]) -> (Vec<bool>, u64) {
    let readers: Vec<(usize, u64)> = depths.iter().map(|&d| (d, u64::MAX)).collect();
    run_reads(row, &readers)
}

/// As [`run`], each reader given as (depth, how many frames it reads before it stops reading).
fn run_reads(row: &Row, readers: &[(usize, u64)]) -> (Vec<bool>, u64) {
    let pool = pool(row.buffers);
    let mut producer = producer(pool.clone(), row);
    let mut keepers: Vec<Keeper> = readers
        .iter()
        .enumerate()
        .map(|(k, &(depth, reads))| Keeper {
            reader: attach(&mut producer, &format!("r{k}")),
            depth,
            held: Vec::new(),
            ended: None,
            reads,
        })
        .collect();
    for id in 0..FRAMES {
        publish(&pool, &mut producer, id);
        let stats = producer.stats();
        assert!(
            stats.outstanding <= row.buffers - row.reserve,
            "B = {}: {} out of the camera's hands at frame {id}",
            row.buffers,
            stats.outstanding
        );
        assert_eq!(stats.bound_unmet, 0);
        for keeper in &mut keepers {
            keeper.step();
        }
    }
    let cut = keepers
        .iter()
        .map(|k| match &k.ended {
            None => false,
            Some(ReadError::Ended(StreamEnd::ConsumerFault)) => true,
            Some(other) => panic!("a reader ended with {other:?}, not a consumer fault"),
        })
        .collect();
    (cut, producer.stats().cut_off_for_reserve)
}

#[test]
fn the_hold_is_b_minus_half_with_two_at_least() {
    for row in &TABLE {
        let hold = row.buffers - (row.buffers / 2).max(2);
        assert_eq!(hold, row.hold, "B = {}", row.buffers);
        let reserve = (row.buffers / 2).max(2).max(row.buffers - hold + 1);
        assert_eq!(reserve, row.reserve, "B = {}", row.buffers);
    }
}

#[test]
fn readers_together_keeping_their_share_are_never_cut() {
    for row in &TABLE {
        // One reader keeping the whole share, then the share split over two readers.
        let (cut, cuts) = run(row, &[row.readers_may_keep]);
        assert_eq!(
            (cut, cuts),
            (vec![false], 0),
            "B = {}: one reader",
            row.buffers
        );
        if row.readers_may_keep >= 2 {
            let a = row.readers_may_keep - 1;
            let (cut, cuts) = run(row, &[a, 1]);
            assert_eq!(
                (cut, cuts),
                (vec![false, false], 0),
                "B = {}: {a} + 1",
                row.buffers
            );
        }
    }
}

#[test]
fn one_frame_past_the_share_cuts_the_oldest_holder_off() {
    for row in &TABLE {
        let (cut, cuts) = run(row, &[row.readers_may_keep + 1]);
        assert_eq!(
            cut,
            vec![true],
            "B = {}: one reader one past the share",
            row.buffers
        );
        assert_eq!(cuts, 1, "B = {}: counted once", row.buffers);
    }
}

#[test]
fn a_reader_keeping_two_beside_one_on_the_same_frames_fits_from_eight() {
    // Both follow the newest frame, so the second reader's frame is one the first holds too: the
    // pair holds two distinct frames, as the first alone does.
    for row in &TABLE {
        let (cut, _) = run(row, &[2, 1]);
        assert_eq!(
            cut.contains(&true),
            row.buffers < 8,
            "B = {}: {cut:?}",
            row.buffers
        );
        let (cut, _) = run(row, &[2]);
        assert_eq!(
            cut == vec![true],
            row.buffers < 8,
            "B = {}: alone",
            row.buffers
        );
    }
}

#[test]
fn a_reader_keeping_two_beside_one_holding_a_frame_of_its_own_needs_nine() {
    // The second reader reads frame 0 and keeps it: three distinct frames held. Below B = 9 the
    // oldest holder (the second reader) is cut off and the first runs on.
    for row in &TABLE {
        let (cut, cuts) = run_reads(row, &[(2, u64::MAX), (1, 1)]);
        // The second reader stops reading, so it never reads its own end: the producer's count
        // says whether it was cut. (first reader cut, readers cut in all)
        let expect = match row.buffers {
            9.. => (false, 0),
            8 => (false, 1),
            // B = 4 and 6: the reader keeping two is past the share alone; both hold frame 0
            // when it is the oldest, so both are cut together.
            _ => (true, 2),
        };
        assert_eq!((cut[0], cuts), expect, "B = {}", row.buffers);
    }
}
