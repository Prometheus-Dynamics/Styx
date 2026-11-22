//! The rkvenc H.264 encoder's job path, on the rk-mpp block of Lemnos run over its played kernel:
//! what Lemnos's own encoder tests run jobs on. Nothing here reaches a device.
//!
//! The job stream is sealed memfds (the stand-ins for its dma-bufs that rk-mpp's tests use), the
//! same descriptors under Styx's pool and under Lemnos's mock stream, so a lease's buffer number is
//! the stream's. Each frame is one job, through `codec.block/1`'s job server and rk-mpp's
//! `MppBlock`, which checks the work the way it does on a board.
//!
//! **The facts are test values** ([`test_facts`]): Lemnos has not stated the VEPU540's encodings
//! (`styx_block::rkvenc::MISSING`), and a test value is not a fact. The played kernel runs no
//! encoder, so the bitstream's length is put in the read-back by [`StandIn`], which stands in for
//! the hardware and is marked as such. What this proves: the job's shape (every register the
//! encoder sets and every buffer it names on the leases), that nothing is copied, that a frame
//! allocates nothing on Styx's side, and the parameter set bytes. Not the encoded picture.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io;
use std::os::fd::BorrowedFd;
use std::sync::{Arc, Mutex};

use lemnos_contract_codec_block::{
    COMPLETION_BYTES, Completion, DATA_OFFSET, JOB_BYTES, Job, JobStatus,
};
use lemnos_core::{Decode, Encode, Reader, Writer};
use lemnos_driver_codec_jobs::{POLL_NS, Server};
use lemnos_driver_rk_mpp::played::Kernel;
use lemnos_driver_rk_mpp::{Block, Device, Message, MppBlock};
use lemnos_driver_rkvenc::layout::{Field, Region};
use lemnos_driver_rkvenc::vepu541_h264::{READ_BACK, ret, set};
use lemnos_hal::mock::MockStream;
use lemnos_linux_sys::dma_buf::{self, sealed_memfd};
use styx_block::Session;
use styx_block::bitstream::{annexb_units, unescape};
use styx_block::h264::ps::{parse_pps, parse_sps};
use styx_block::rkvenc::{
    self, EncoderFacts, MISSING, RkvencH264, Scratch, TAG_READ, TAG_WRITE, vepu540_facts,
};
use styx_codec_api::{CodecError, CodecKind, Encoder, FourCc, Registry, Request, Verdict};
use styx_format::{MediaFormat, Resolution};
use styx_frame::{
    BufferMemory, BufferSize, FdMemory, FrameEntry, FramePool, Layouts, PlaneLayout, copy_counters,
    packed_layouts,
};

// ---------------------------------------------------------------------------------------------
// Counting Styx's allocations, per thread, while a test turns it on.

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn count() {
    let _ = COUNTING.try_with(|on| {
        if on.get() {
            let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        }
    });
}

// SAFETY: forwards every call to the system allocator unchanged; only adds a counter.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller upholds GlobalAlloc::alloc's contract; System is the allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller passes back a pointer and layout System handed out.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: as dealloc; System reallocates the block the caller owns.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTER: Counting = Counting;

/// Runs `f` with counting off: the stand-in block's own allocations are Lemnos's job server and
/// mock stream, not Styx's.
fn uncounted<T>(f: impl FnOnce() -> T) -> T {
    let was = COUNTING.with(|on| on.replace(false));
    let out = f();
    COUNTING.with(|on| on.set(was));
    out
}

// ---------------------------------------------------------------------------------------------
// The played kernel, and the stand-in for the hardware.

/// The bitstream length the stand-in reports for each job: what a block would put in `BS_LGTH`.
/// A stand-in's number, not a measurement.
const STAND_IN_SLICE_BYTES: u32 = 1000;

/// The played kernel, with the hardware's one result stood in: the session's read-back is the
/// kernel's, with `BS_LGTH` set to [`STAND_IN_SLICE_BYTES`]. **A stand-in for the hardware.**
struct StandIn(Arc<Mutex<Kernel>>);

impl lemnos_driver_rk_mpp::Session for StandIn {
    fn send(&mut self, messages: &mut [Message<'_>]) -> io::Result<()> {
        self.0.lock().unwrap().call(messages)
    }

    fn read_back(&mut self, slot: usize, into: &mut [u8]) {
        let n = into.len();
        into.copy_from_slice(&self.0.lock().unwrap().areas[slot][..n]);
        let at = 4 * usize::from(ret::st_bsl::BS_LGTH.word - READ_BACK.first);
        if let Some(word) = into.get_mut(at..at + 4) {
            word.copy_from_slice(&STAND_IN_SLICE_BYTES.to_le_bytes());
        }
    }
}

struct PlayedDevice(Arc<Mutex<Kernel>>);

impl Device for PlayedDevice {
    type Session = StandIn;

    fn open(&mut self) -> io::Result<StandIn> {
        let mut kernel = self.0.lock().unwrap();
        kernel.opens += 1;
        kernel.client = None;
        Ok(StandIn(Arc::clone(&self.0)))
    }

    fn confined(&mut self, block: Block) -> io::Result<bool> {
        let mut kernel = self.0.lock().unwrap();
        kernel.bound = Some(block);
        Ok(kernel.confined)
    }

    fn buffer_size(&mut self, fd: BorrowedFd<'_>) -> io::Result<u64> {
        dma_buf::size(fd)
    }
}

// ---------------------------------------------------------------------------------------------
// The job stream: Styx's pool and Lemnos's mock stream over the same memfds.

const BUFFERS: usize = 8;
const BYTES: usize = 16 * 1024;
/// The stream buffers the test session keeps for the work and the result; no lease takes them.
const WORK: u32 = 6;
const RESULT: u32 = 7;
/// The buffer the block's own writes go to, held by a lease for the encoder's life.
const SCRATCH: usize = 3;

/// A `styx_block::Session` that runs each job on rk-mpp's block through the job server.
struct PlayedSession {
    server: Server,
    block: MppBlock<PlayedDevice>,
    stream: MockStream,
    consumer: MockStream,
    now: u64,
    identity: u64,
    /// Every job's status and read-back count.
    log: Arc<Mutex<Vec<(JobStatus, u32)>>>,
}

impl Session for PlayedSession {
    fn run(&mut self, work: &[u8]) -> Result<u32, CodecError> {
        self.run_reading(work, &mut [])
    }

    fn run_reading(&mut self, work: &[u8], read_back: &mut [u8]) -> Result<u32, CodecError> {
        uncounted(|| {
            self.consumer.fill(WORK, work);
            self.identity += 1;
            let job = Job {
                identity: self.identity,
                work: WORK,
                work_bytes: work.len() as u32,
                result: RESULT,
            };
            let mut encoded = [0u8; JOB_BYTES];
            job.encode(&mut Writer::new(&mut encoded)).unwrap();
            self.consumer.post_job(&encoded);
            self.now += POLL_NS;
            self.server
                .serve(&mut self.stream, &mut self.block, self.now)
                .unwrap();
            let frame = self.consumer.take_frame().expect("a completion");
            assert_eq!((frame.identity, frame.buffer), (self.identity, RESULT));
            let contents = self.consumer.contents(RESULT);
            let record =
                Completion::decode(&mut Reader::new(&contents[..COMPLETION_BYTES])).unwrap();
            self.consumer.return_buffer(RESULT).unwrap();
            self.log.lock().unwrap().push((record.status, record.count));
            match record.status {
                JobStatus::Done => {
                    let count = record.count as usize;
                    read_back
                        .get_mut(..count)
                        .ok_or_else(|| CodecError::Failed("read-back too long".into()))?
                        .copy_from_slice(&contents[DATA_OFFSET..DATA_OFFSET + count]);
                    Ok(record.count)
                }
                JobStatus::Refused => Err(CodecError::Rejected("the block refused".into())),
                other => Err(CodecError::Failed(format!("the job ended {other:?}"))),
            }
        })
    }
}

struct Rig {
    pool: FramePool<usize>,
    kernel: Arc<Mutex<Kernel>>,
    log: Arc<Mutex<Vec<(JobStatus, u32)>>>,
    session: Option<PlayedSession>,
}

fn all_blocks() -> u32 {
    Block::ALL
        .into_iter()
        .fold(0, |bits, b| bits | 1 << b.facts().client)
}

impl Rig {
    fn new() -> Self {
        let fds: Vec<_> = (0..BUFFERS)
            .map(|_| sealed_memfd(c"styx-rkvenc-test", BYTES as u64).unwrap())
            .collect();
        let memories = fds.iter().map(|fd| {
            Box::new(FdMemory::memfd(fd.try_clone().unwrap(), BYTES, true).unwrap())
                as Box<dyn BufferMemory>
        });
        let pool = FramePool::new(BufferSize::new(BYTES, 0), memories).unwrap();
        let stream = MockStream::with_handles(fds);
        let kernel = Arc::new(Mutex::new(Kernel::new(all_blocks())));
        let log = Arc::new(Mutex::new(Vec::new()));
        let session = PlayedSession {
            server: Server::new(),
            block: MppBlock::new(PlayedDevice(Arc::clone(&kernel)), Block::Rkvenc),
            consumer: stream.clone(),
            stream,
            now: 0,
            identity: 0,
            log: Arc::clone(&log),
        };
        Self {
            pool,
            kernel,
            log,
            session: Some(session),
        }
    }

    fn encoder(&mut self, facts: EncoderFacts) -> Result<RkvencH264, CodecError> {
        let mut once = self.session.take();
        RkvencH264::new(
            move || {
                once.take()
                    .map(|s| Box::new(s) as Box<dyn Session>)
                    .ok_or_else(|| {
                        CodecError::Unavailable("the test's one session is in use".into())
                    })
            },
            facts,
            (SCRATCH as u32, BYTES as u32),
            30,
        )
    }
}

// ---------------------------------------------------------------------------------------------
// The test values.

const fn picture(width: u32, height: u32) -> u32 {
    width * height * 3 / 2
}

const fn quarter(width: u32, height: u32) -> u32 {
    width * height / 4
}

/// The block's own writes, with sizes made up for a small picture. TEST VALUES.
const TEST_SCRATCH: [Scratch; 3] = [
    Scratch {
        register: set::reg074::RFPW_H_ADDR,
        bytes: picture,
    },
    Scratch {
        register: set::reg075::RFPW_B_ADDR,
        bytes: quarter,
    },
    Scratch {
        register: set::reg080::DSPW_ADDR,
        bytes: quarter,
    },
];

/// One field with no H.264 meaning, set to a made-up value. TEST VALUES.
const TEST_START: [(Field, i64); 1] = [(set::reg013::LOG2_CTU_NUM, 3)];

/// **TEST VALUES, not facts.** Lemnos has stated none of these (`rkvenc::MISSING`); each is made
/// up so that the job path runs, and none may leave this file.
fn test_facts() -> EncoderFacts {
    EncoderFacts {
        source_nv12: 0,
        standard_h264: 1,
        bitstream_base: set::reg084::BSBB_ADDR,
        bitstream_top: set::reg083::BSBT_ADDR,
        bitstream_read: set::reg085::BSBR_ADDR,
        bitstream_start: set::reg086::ADR_BSBS,
        writes_slice_header: true,
        fill: |size| i64::from(size.next_multiple_of(8) - size),
        scratch: &TEST_SCRATCH,
        start: &TEST_START,
    }
}

const WIDTH: u32 = 64;
const HEIGHT: u32 = 32;

fn nv12() -> MediaFormat {
    MediaFormat::with_defaults(FourCc::NV12, Resolution::new(WIDTH, HEIGHT).unwrap())
}

fn field(encoder: &RkvencH264, field: Field) -> u32 {
    field
        .read(Region::Set, encoder.registers().words(), 0)
        .expect("a write-side field")
}

// ---------------------------------------------------------------------------------------------

#[test]
fn the_encoder_refuses_to_build_without_lemnos_facts() {
    let error = vepu540_facts().expect_err("Lemnos states no VEPU540 facts yet");
    let CodecError::Unavailable(text) = &error else {
        panic!("{error:?}");
    };
    assert!(text.contains("not usable until Lemnos supplies the VEPU540 encodings"));
    for fact in MISSING {
        assert!(text.contains(fact), "{fact}");
    }

    // The registry candidate is built with Lemnos's facts, so it does not build either.
    let candidate = rkvenc::candidate::<usize>(
        || Err(CodecError::Unavailable("no block in this test".into())),
        (0, 0),
        30,
    );
    let mut registry = Registry::<usize>::new();
    registry.register(candidate);
    let error = registry
        .select(Request::new(CodecKind::Encoder, FourCc::NV12, FourCc::H264))
        .expect_err("built without facts");
    let [decision] = &error.decisions[..] else {
        panic!("{error}");
    };
    let Verdict::Skipped(why) = &decision.verdict else {
        panic!("{decision}");
    };
    assert!(why.contains("VEPU540"), "{why}");

    // A block that does not write the slice header is refused: this encoder writes none.
    let mut rig = Rig::new();
    let facts = EncoderFacts {
        writes_slice_header: false,
        ..test_facts()
    };
    assert!(matches!(rig.encoder(facts), Err(CodecError::Rejected(_))));
}

#[test]
fn the_work_items_are_rk_mpps() {
    assert_eq!(TAG_WRITE, lemnos_driver_rk_mpp::codec::TAG_WRITE);
    assert_eq!(TAG_READ, lemnos_driver_rk_mpp::codec::TAG_READ);
    assert_eq!(
        rkvenc::BLOCK,
        lemnos_driver_rk_mpp::codec::block_name(Block::Rkvenc)
    );
}

#[test]
fn frames_run_as_jobs_on_the_leases_with_no_copy_and_no_allocation() {
    let mut rig = Rig::new();
    let mut encoder = rig.encoder(test_facts()).expect("an encoder on test facts");

    // The block's own writes go to a buffer the caller holds for the encoder's life.
    let scratch_layout = Layouts::new(&[PlaneLayout::new(0, BYTES, BYTES)]).unwrap();
    let _scratch = rig
        .pool
        .acquire_at(SCRATCH, FrameEntry::new(0, 0, nv12(), scratch_layout))
        .expect("the scratch buffer");

    let bitstream =
        MediaFormat::with_defaults(FourCc::H264, Resolution::new(WIDTH, HEIGHT).unwrap());
    let room = 8192usize;
    let mut idr = Vec::new();
    for frame in 0..4u64 {
        let measuring = frame >= 2;
        let mut input = rig
            .pool
            .acquire_at(
                0,
                FrameEntry::new(
                    frame,
                    1_000 * frame,
                    nv12(),
                    packed_layouts(FourCc::NV12, WIDTH, HEIGHT, None, None).unwrap(),
                ),
            )
            .expect("the picture");
        input.write().unwrap().bytes()[..(picture(WIDTH, HEIGHT) as usize)].fill(0x80);
        let mut output = rig
            .pool
            .acquire_at(
                1,
                FrameEntry::new(
                    frame,
                    1_000 * frame,
                    bitstream,
                    Layouts::new(&[PlaneLayout::new(0, room, room)]).unwrap(),
                ),
            )
            .expect("the bitstream room");

        if measuring {
            ALLOCATIONS.with(|n| n.set(0));
            COUNTING.with(|on| on.set(true));
        }
        let ran = encoder.encode(&input, &mut output);
        COUNTING.with(|on| on.set(false));
        ran.expect("the job runs");
        if measuring {
            assert_eq!(
                ALLOCATIONS.with(Cell::get),
                0,
                "frame {frame} allocated on Styx's side"
            );
        }

        // The output: H.264, the parameter sets, then the block's slice of the stand-in's length.
        let out = output.entry();
        assert_eq!(out.format.code, FourCc::H264);
        assert_eq!((out.id, out.timestamp_ns), (frame, 1_000 * frame));
        let read = output.read().unwrap();
        let bytes = read.plane(0).unwrap().data;
        let header = bytes.len() - STAND_IN_SLICE_BYTES as usize;
        check_parameter_sets(&bytes[..header]);

        // The job: every buffer it names is a lease's, at the offset the lease's layout gives.
        let refs = encoder.registers().stream_refs();
        let named = |reg: u16| {
            refs.iter()
                .find(|r| r.register == reg)
                .map(|r| (r.buffer, r.offset))
        };
        let uv = packed_layouts(FourCc::NV12, WIDTH, HEIGHT, None, None).unwrap()[1].offset as u32;
        assert_eq!(named(set::reg070::ADR_SRC0.word), Some((0, 0)));
        assert_eq!(named(set::reg071::ADR_SRC1.word), Some((0, uv)));
        assert_eq!(named(set::reg084::BSBB_ADDR.word), Some((1, 0)));
        assert_eq!(named(set::reg085::BSBR_ADDR.word), Some((1, 0)));
        assert_eq!(named(set::reg083::BSBT_ADDR.word), Some((1, room as u32)));
        assert_eq!(named(set::reg086::ADR_BSBS.word), Some((1, header as u32)));
        assert_eq!(
            named(set::reg074::RFPW_H_ADDR.word),
            Some((SCRATCH as u32, 0))
        );
        assert_eq!(
            named(set::reg075::RFPW_B_ADDR.word),
            Some((SCRATCH as u32, 4096))
        );
        assert_eq!(
            named(set::reg080::DSPW_ADDR.word),
            Some((SCRATCH as u32, 4096 + 1024))
        );
        assert_eq!(refs.len(), 9);

        // The registers Styx sets.
        assert_eq!(field(&encoder, set::reg012::PIC_WD8_M1), WIDTH / 8 - 1);
        assert_eq!(field(&encoder, set::reg012::PIC_HD8_M1), HEIGHT / 8 - 1);
        assert_eq!(field(&encoder, set::reg013::PIC_QP), 30);
        assert_eq!(field(&encoder, set::reg069::SRC_STRD0), WIDTH);
        assert_eq!(field(&encoder, set::reg069::SRC_STRD1), WIDTH);
        assert_eq!(field(&encoder, set::reg103::NAL_UNIT_TYPE), 5);
        assert_eq!(field(&encoder, set::reg106::SLI_TYPE), 2);
        assert_eq!(field(&encoder, set::reg105::PIC_INIT_QP), 26);
        assert_eq!(field(&encoder, set::reg013::LOG2_CTU_NUM), 3);
        idr.push(field(&encoder, set::reg107::IDR_PIC_ID));
    }
    // Two IDRs in a row differ in idr_pic_id.
    assert_eq!(idr, [0, 1, 0, 1]);

    // Every job ran, and the read-back was the block's window.
    let log = rig.log.lock().unwrap();
    assert_eq!(log.len(), 4);
    for &(status, count) in log.iter() {
        assert_eq!(status, JobStatus::Done);
        assert_eq!(count, 4 * u32::from(READ_BACK.count));
    }
    assert!(rig.kernel.lock().unwrap().calls > 0);

    for counter in copy_counters() {
        assert_eq!(counter.copies(), 0, "{} copied", counter.site());
    }
}

#[test]
fn inputs_the_block_cannot_take_are_refused_before_a_job() {
    let mut rig = Rig::new();
    let mut encoder = rig.encoder(test_facts()).expect("an encoder on test facts");
    let bitstream =
        MediaFormat::with_defaults(FourCc::H264, Resolution::new(WIDTH, HEIGHT).unwrap());
    let yuyv = MediaFormat::with_defaults(FourCc::YUYV, Resolution::new(WIDTH, HEIGHT).unwrap());

    let input = rig
        .pool
        .acquire_at(
            0,
            FrameEntry::new(
                0,
                0,
                yuyv,
                packed_layouts(FourCc::YUYV, WIDTH, HEIGHT, None, None).unwrap(),
            ),
        )
        .unwrap();
    let mut output = rig
        .pool
        .acquire_at(
            1,
            FrameEntry::new(
                0,
                0,
                bitstream,
                Layouts::new(&[PlaneLayout::new(0, 4096, 4096)]).unwrap(),
            ),
        )
        .unwrap();
    assert!(matches!(
        encoder.encode(&input, &mut output),
        Err(CodecError::Rejected(_))
    ));
    drop((input, output));

    // An output plane off the base register's alignment.
    let input = rig
        .pool
        .acquire_at(
            0,
            FrameEntry::new(
                0,
                0,
                nv12(),
                packed_layouts(FourCc::NV12, WIDTH, HEIGHT, None, None).unwrap(),
            ),
        )
        .unwrap();
    let mut output = rig
        .pool
        .acquire_at(
            1,
            FrameEntry::new(
                0,
                0,
                bitstream,
                Layouts::new(&[PlaneLayout::new(64, 4096, 4096)]).unwrap(),
            ),
        )
        .unwrap();
    assert!(matches!(
        encoder.encode(&input, &mut output),
        Err(CodecError::Rejected(_))
    ));
    assert!(
        rig.log.lock().unwrap().is_empty(),
        "no job reached the block"
    );
}

/// The SPS and PPS Styx wrote: two NAL units, parsed back with this crate's own H.264 parser, in
/// the shape `styx_h264::SHAPE` states.
fn check_parameter_sets(bytes: &[u8]) {
    let shape = styx_h264::SHAPE;
    let mut rbsp = [0u8; 64];
    let units: Vec<&[u8]> = annexb_units(bytes).collect();
    assert_eq!(units.len(), 2, "an SPS and a PPS");
    assert_eq!(units[0][0] & 0x1f, 7);
    assert_eq!(units[1][0] & 0x1f, 8);
    let n = unescape(units[0], &mut rbsp).unwrap();
    let sps = parse_sps(&rbsp[..n]).expect("an SPS");
    assert_eq!(sps.profile_idc, shape.profile_idc);
    assert_eq!(u32::from(sps.poc_type), shape.pic_order_cnt_type);
    assert_eq!(u32::from(sps.log2_max_frame_num), shape.log2_max_frame_num);
    assert_eq!(sps.frame_size(), (WIDTH, HEIGHT));
    let n = unescape(units[1], &mut rbsp).unwrap();
    let pps = parse_pps(&rbsp[..n], |_| Some(1)).expect("a PPS");
    assert_eq!(pps.cabac, shape.cabac);
    assert_eq!(pps.pic_init_qp, i32::from(shape.pic_init_qp));
    assert_eq!(
        pps.deblocking_filter_control_present,
        shape.deblocking_filter_control_present
    );
    assert_eq!(pps.constrained_intra_pred, shape.constrained_intra_pred);
}
