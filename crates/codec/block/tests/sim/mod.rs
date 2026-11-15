//! A simulated `codec.block/1` block for the live session's test. Lemnos has no simulated codec
//! block at its lock (`04d09360`: the block drivers test on played-back kernels, not on
//! `lemnos-sim`), so this one is written here. It speaks the contract and nothing more:
//!
//! - It takes every buffer of its job stream from the consumer (`next_buffer`), and holds each
//!   one it has not published.
//! - A job is read once from its work buffer and parsed with the contract's `Work`. A work that
//!   does not parse, or names a buffer it does not hold, completes as refused and nothing else
//!   happens.
//! - The bitstream's first NAL unit must start with a start code, and the picture is written
//!   with a pattern of the job's stamp, so the consumer can check what the block was given.
//! - The completion is a record at the result buffer's start and a frame on the job stream with
//!   the job's identity.
//!
//! What the block records about each job is kept in [`JOBS`], for the test to read.
//!
//! The block takes a returned buffer when it looks at the stream, before each job it reads (the
//! contract's `next_buffer`), as Lemnos's `drivers/codec-jobs` server does at the pinned rev.
//! [`STALE_REFRESH`] is the OLD behaviour, one look per serve: a job read later in the serve can
//! then name a buffer returned after that look, and the block refuses it. [`AWAIT_JOBS`] makes a
//! serve wait for the next job after it publishes a completion, so a test can post the next job
//! at the moment it chooses.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config as ServiceConfig, Service};
use styx_block::hevc;

use lemnos_contract_codec_block::work::Work;
use lemnos_contract_codec_block::{
    BlockInfo, BufferRef, COMPLETION_BYTES, CodecBlock, Completion, Counters, DATA_OFFSET,
    JOB_BYTES, Job, JobStatus,
};
use lemnos_core::{
    Config, Decode, Encode, Lifecycle, Reader, Result, Status, StreamHandle, Writer, driver,
};
use lemnos_hal::blocks::StreamFrame;

/// The block's name. The work the decoder builds is the one `v4l2-stateless` takes.
pub const NAME: &str = "v4l2-stateless";
/// The item of the setup, and the item of the stamp (`styx_block::work`).
const TAG_SETUP: u32 = 1;
const TAG_STAMP: u32 = 2;
/// The reference slots of the bitstream and the picture (`styx_block::work`).
const SLOT_SOURCE: u32 = 0;
const SLOT_PICTURE: u32 = 1;

/// One job as the block saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub identity: u64,
    /// The buffer the job's result was to be written to.
    pub result: u32,
    pub refs: Vec<BufferRef>,
    pub items: Vec<(u32, Vec<u8>)>,
    pub start_code: bool,
    pub outcome: JobStatus,
}

/// Every job the block has seen, in order.
pub static JOBS: Mutex<Vec<Seen>> = Mutex::new(Vec::new());

/// Whether the block looks at returned buffers once per serve only (see the module's notes).
pub static STALE_REFRESH: AtomicBool = AtomicBool::new(false);
/// Jobs a serve waits for after it publishes a completion: each one is spent when a completion
/// is published, and the serve waits for the job after it (up to [`AWAIT_LIMIT`]).
pub static AWAIT_JOBS: AtomicU32 = AtomicU32::new(0);
/// How long a serve waits for the job it awaits before it gives up.
const AWAIT_LIMIT: Duration = Duration::from_secs(10);
/// Jobs the block refuses outright, whatever they name: a block that cannot take a job now.
pub static REFUSE_JOBS: AtomicU32 = AtomicU32::new(0);

/// The bitstream of the fixture: two IDR frames' worth of parameter sets and slices.
pub const STREAM: &[u8] = include_bytes!("../../src/hevc/fixtures/idr320.h265");

/// The board the tests run on: one `codec.block/1` device with eight job-stream buffers. Its jobs
/// are the contract's `JOB_BYTES`, so the board follows the pinned contract's record size.
pub fn board() -> String {
    format!(
        "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-codec\"\n\
         [[devices]]\nid = \"codec\"\ndriver = \"sim-codec\"\n\
         [devices.streams.jobs]\nbuffers = 8\nbuffer_bytes = 131072\njob_slots = 8\njob_bytes = {JOB_BYTES}\n"
    )
}

/// A directory for a test's socket, under the temp directory.
pub fn socket_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-block-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Starts `lemnosd` in this process with the simulated block on its board. The socket is in a
/// directory named after `name`.
pub fn start(name: &str) -> (Service, PathBuf) {
    let dir = socket_dir(name);
    let socket = dir.join("sock");
    let config = ServiceConfig {
        socket: socket.clone(),
        name: "styx-block-test".to_string(),
        board: parse(&board()).expect("board"),
        drivers: vec![entry::<SimCodec>()],
        backend: Box::new(SimBoard::new()),
        options: Options {
            call_deadline: Duration::from_millis(500),
            ..Options::default()
        },
    };
    (Service::start(config).expect("start lemnosd"), socket)
}

/// The access unit of one IDR frame: the stream's IDR slice, with its start code, first.
pub fn idr_access_unit() -> Vec<u8> {
    let (_, raw) = hevc::nal_units(STREAM)
        .map(|nal| (nal.unit_type, nal.raw.to_vec()))
        .find(|(unit_type, _)| *unit_type == 19)
        .expect("the fixture has an IDR slice");
    let mut au = vec![0, 0, 1];
    au.extend_from_slice(&raw);
    au
}

/// What the picture holds at `index` for a job stamped `stamp`.
#[must_use]
pub fn pattern(index: usize, stamp: u64) -> u8 {
    (((index as u64).wrapping_mul(31)).wrapping_add(stamp) & 0xff) as u8
}

#[driver("sim-codec", provides = [CodecBlock])]
pub struct SimCodec {
    #[needs(job)]
    jobs: StreamHandle,
    /// The buffers the block holds, one bit each.
    held: u64,
    counters: Counters,
}

impl Lifecycle for SimCodec {
    type Needs = SimCodecNeeds;
    const TICKS: bool = true;

    fn start(needs: SimCodecNeeds, _cfg: &Config<'_>) -> Result<Self> {
        Ok(Self {
            jobs: needs.jobs,
            held: 0,
            counters: Counters::default(),
        })
    }

    fn tick(&mut self, now_ns: u64) -> Option<u64> {
        self.serve(now_ns);
        Some(now_ns.saturating_add(1_000_000))
    }
}

impl SimCodec {
    fn hold(&mut self, buffer: u32) {
        if buffer < 64 {
            self.held |= 1 << buffer;
        }
    }

    fn holds(&self, buffer: u32) -> bool {
        buffer < 64 && self.held & (1 << buffer) != 0
    }

    /// Takes back every buffer the consumer has returned. A buffer taken for a consumer that has
    /// gone is given up first: published, it reaches no one (the endpoint's epoch says so).
    fn refresh(&mut self) {
        if self.held != 0
            && !self
                .jobs
                .buffer_current(self.held.trailing_zeros())
                .unwrap_or(false)
        {
            while self.held != 0 {
                let buffer = self.held.trailing_zeros();
                let _ = self.jobs.publish(StreamFrame {
                    buffer,
                    bytes: 0,
                    sequence: 0,
                    identity: 0,
                    timestamp_ns: 0,
                });
                self.held &= !(1 << buffer);
            }
        }
        for _ in 0..self.jobs.buffer_count() {
            match self.jobs.next_buffer() {
                Ok(Some(buffer)) => self.hold(buffer),
                _ => break,
            }
        }
    }

    fn serve(&mut self, now_ns: u64) {
        self.refresh();
        let mut bytes = [0u8; 64];
        // The deadline of the job this serve awaits, while it awaits one.
        let mut awaiting: Option<Instant> = None;
        loop {
            let len = match self.jobs.next_job(&mut bytes) {
                Ok(Some(len)) => len,
                Ok(None) => match awaiting {
                    Some(deadline) if Instant::now() < deadline => {
                        std::thread::yield_now();
                        continue;
                    }
                    _ => return,
                },
                Err(_) => return,
            };
            let waited = awaiting.take().is_some();
            if !STALE_REFRESH.load(Ordering::SeqCst) {
                self.refresh();
            }
            let Ok(job) = Job::decode(&mut Reader::new(&bytes[..len])) else {
                let _ = self.jobs.fault_consumer();
                return;
            };
            if !self.holds(job.result) {
                // A result the block does not hold is a broken consumer: fault the stream.
                let _ = self.jobs.fault_consumer();
                return;
            }
            let _ = self.run(&job, now_ns);
            self.held &= !(1 << job.result);
            if waited && STALE_REFRESH.load(Ordering::SeqCst) {
                return;
            }
            if AWAIT_JOBS
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                awaiting = Some(Instant::now() + AWAIT_LIMIT);
            }
        }
    }

    /// Runs one job and publishes its completion on the job's result buffer.
    fn run(&mut self, job: &Job, now_ns: u64) -> JobStatus {
        let (status, seen) = self.decode_job(job);
        if let Some(seen) = seen {
            JOBS.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(seen);
        }
        // No read-back: the picture is the job's result, and the record says it is done.
        let record = Completion { status, count: 0 };
        let mut out = [0u8; COMPLETION_BYTES];
        if record.encode(&mut Writer::new(&mut out)).is_ok() {
            let _ = self.jobs.write(job.result, 0, &out);
        }
        let _ = self.jobs.publish(StreamFrame {
            buffer: job.result,
            bytes: DATA_OFFSET as u32,
            sequence: self.counters.jobs + self.counters.refused + self.counters.failed,
            identity: job.identity,
            timestamp_ns: now_ns,
        });
        match status {
            JobStatus::Done => self.counters.jobs += 1,
            JobStatus::Refused => self.counters.refused += 1,
            JobStatus::Failed => self.counters.failed += 1,
        }
        status
    }

    /// The job's status, and what the block saw of it once the work parsed.
    fn decode_job(&mut self, job: &Job) -> (JobStatus, Option<Seen>) {
        if REFUSE_JOBS
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            let seen = Seen {
                identity: job.identity,
                result: job.result,
                refs: Vec::new(),
                items: Vec::new(),
                start_code: false,
                outcome: JobStatus::Refused,
            };
            return (JobStatus::Refused, Some(seen));
        }
        let mut work = vec![0u8; job.work_bytes as usize];
        if !self.holds(job.work) || !matches!(self.jobs.read(job.work, 0, &mut work), Ok(true)) {
            return (JobStatus::Refused, None);
        }
        let Some(parsed) = Work::parse(&work) else {
            return (JobStatus::Refused, None);
        };
        let refs: Vec<BufferRef> = parsed.refs().collect();
        let items: Vec<(u32, Vec<u8>)> = parsed.items().map(|(t, b)| (t, b.to_vec())).collect();
        let named_ok = refs
            .iter()
            .all(|r| self.holds(r.buffer) && r.buffer != job.result);
        let source = refs.iter().find(|r| r.slot == SLOT_SOURCE).copied();
        let picture = refs.iter().find(|r| r.slot == SLOT_PICTURE).copied();
        let setup = items
            .iter()
            .find(|(t, _)| *t == TAG_SETUP)
            .map(|(_, b)| b.clone());
        let stamp = items
            .iter()
            .find(|(t, _)| *t == TAG_STAMP)
            .map(|(_, b)| b.clone());

        let mut start_code = false;
        let mut status = JobStatus::Refused;
        // The setup is five `u32`s (coded, width, height, picture, planes) and the stamp a `u64`.
        if let (true, Some(source), Some(picture), Some(setup), Some(stamp)) =
            (named_ok, source, picture, setup, stamp)
            && let (Some(setup), Some(stamp)) =
                (setup.first_chunk::<20>(), stamp.first_chunk::<8>())
        {
            let width = u32::from_le_bytes(setup[4..8].try_into().unwrap_or([0; 4])) as usize;
            let height = u32::from_le_bytes(setup[8..12].try_into().unwrap_or([0; 4])) as usize;
            let stamp = u64::from_le_bytes(*stamp);
            let mut head = [0u8; 3];
            if matches!(self.jobs.read(source.buffer, 0, &mut head), Ok(true)) {
                start_code = head == [0, 0, 1];
            }
            let total = width * height * 3 / 2;
            if start_code && total > 0 {
                let bytes: Vec<u8> = (0..total).map(|i| pattern(i, stamp)).collect();
                if matches!(self.jobs.write(picture.buffer, 0, &bytes), Ok(true)) {
                    status = JobStatus::Done;
                }
            }
        }
        let seen = Seen {
            identity: job.identity,
            result: job.result,
            refs,
            items,
            start_code,
            outcome: status,
        };
        (status, Some(seen))
    }
}

fn name_into(name: &str, into: &mut [u8]) -> Result<usize> {
    into.get_mut(..name.len())
        .ok_or(Status::INVALID_ARGUMENT)?
        .copy_from_slice(name.as_bytes());
    Ok(name.len())
}

impl CodecBlock for SimCodec {
    fn info(&mut self) -> Result<BlockInfo> {
        Ok(BlockInfo {
            max_work_bytes: 4096,
            max_refs: 8,
            max_items: 32,
            max_read_back: 0,
            max_buffers: 8,
        })
    }

    fn block(&mut self, into: &mut [u8]) -> Result<usize> {
        name_into(NAME, into)
    }

    fn job_stream(&mut self, into: &mut [u8]) -> Result<usize> {
        name_into("jobs", into)
    }

    fn counters(&mut self) -> Result<Counters> {
        Ok(self.counters)
    }
}
