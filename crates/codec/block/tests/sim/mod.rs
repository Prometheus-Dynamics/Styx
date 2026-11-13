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

use std::sync::{Mutex, PoisonError};

use lemnos_contract_codec_block::work::Work;
use lemnos_contract_codec_block::{
    BlockInfo, BufferRef, COMPLETION_BYTES, CodecBlock, Completion, Counters, DATA_OFFSET, Job,
    JobStatus,
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
    pub refs: Vec<BufferRef>,
    pub items: Vec<(u32, Vec<u8>)>,
    pub start_code: bool,
    pub outcome: JobStatus,
}

/// Every job the block has seen, in order.
pub static JOBS: Mutex<Vec<Seen>> = Mutex::new(Vec::new());

/// What the picture holds at `index` for a job stamped `stamp`.
#[must_use]
pub fn pattern(index: usize, stamp: u64) -> u8 {
    (((index as u64).wrapping_mul(31)).wrapping_add(stamp) & 0xff) as u8
}

#[driver("sim-codec", provides = [CodecBlock])]
pub struct SimCodec {
    #[needs]
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
        while let Ok(Some(len)) = self.jobs.next_job(&mut bytes) {
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
