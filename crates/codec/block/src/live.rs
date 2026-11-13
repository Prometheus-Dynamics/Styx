//! The live [`Session`]: a decode job posted on a `codec.block/1` device's job stream through
//! `lemnos-client`, and its completion waited for on the same stream.
//!
//! [`LiveSession::open`] finds the job stream by the device's own `job_stream` call and opens it
//! writable (the consumer writes the bitstream and the work, so the stream is mapped for writing).
//! Its buffers become one [`FramePool`]: the caller takes the bitstream and picture leases from
//! it with [`FramePool::acquire_at`], and the session takes the job's work and result buffers from
//! the same pool, the ones the job does not name and no lease holds.
//!
//! One job runs at a time: the session writes the work into its buffer, posts a [`Job`] naming
//! the work, the result buffer and the work's length, and waits for the completion frame that
//! carries the job's identity. Nothing is copied but the work, which the consumer writes once.
//!
//! Lemnos does the mapping and the ring; the block does the decode. The completion's record says
//! how the job ended and how many bytes of read-back follow it. A refused job is
//! [`CodecError::Rejected`], and the session stays open.
//!
//! What is not here: a reopen after a failure makes a new stream, so new memory. The leases taken
//! from the old pool still point at it, so the caller must take new ones from the new session's
//! pool. [`LiveSession::pool`] says which pool is the session's.
//!
//! Provenance: the job stream protocol is Lemnos's `codec.block/1` (`contracts/codec-block`, and
//! `drivers/codec-jobs` for the block side). The writable mapping of the stream is
//! `styx_lemnos::open_job_stream`; this module only says what a job is.

use std::fmt;
use std::time::Duration;

use lemnos_contract_codec_block::work::Work;
use lemnos_contract_codec_block::{
    COMPLETION_BYTES, CodecBlock, Completion, DATA_OFFSET, JOB_BYTES, Job, JobStatus,
    MAX_STREAM_NAME,
};
use lemnos_core::{Decode, Encode, Reader, Writer};
use styx_codec_api::{CodecError, FourCc};
use styx_format::{MediaFormat, Resolution};
use styx_frame::{FrameEntry, Layouts, PlaneLayout};
use styx_lemnos::{Client, DeviceEntry, HeldFrame, JobStream, open_job_stream};

use crate::decoder::Session;

/// The tag of a codec block device.
pub const CODEC_TAG: &str = "codec.block/1";

/// Most buffers one job can name. The contract's job stream holds at most this many.
const MAX_NAMED: usize = 64;

/// A `codec.block/1` device's job stream as a [`Session`]. Its buffers are the job stream's pool.
pub struct LiveSession {
    stream: JobStream<usize>,
    /// Identity of the next job. Only the completion with this number is the job's.
    sequence: u64,
    timeout: Duration,
    /// Jobs not posted because the pool had no free buffer for the work or the result, or the job
    /// queue was full. Nothing was sent, so the block never saw them.
    refused: u64,
}

impl fmt::Debug for LiveSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveSession")
            .field("stream", &self.stream.name())
            .field("buffers", &self.stream.buffers())
            .field("sequence", &self.sequence)
            .finish()
    }
}

impl LiveSession {
    /// Opens `device`'s job stream, writable, and maps its buffers into a pool. `timeout` bounds
    /// the wait for each job's completion.
    ///
    /// The device must answer `job_stream` with a name the service has. An error is
    /// [`CodecError::Unavailable`]: the device or its stream is not there to run a job.
    pub fn open(
        client: &Client,
        device: &DeviceEntry,
        timeout: Duration,
    ) -> Result<Self, CodecError> {
        let name = job_stream_name(client, device)?;
        let stream = open_job_stream::<usize>(client, device.index, &name)
            .map_err(|e| CodecError::Unavailable(format!("job stream {name}: {e}")))?;
        if stream.job_bytes() < JOB_BYTES {
            return Err(CodecError::Unavailable(format!(
                "job stream {name}: jobs are {} bytes, the contract's are {JOB_BYTES}",
                stream.job_bytes()
            )));
        }
        Ok(Self {
            stream,
            sequence: 0,
            timeout,
            refused: 0,
        })
    }

    /// The pool over the job stream's buffers. Take the bitstream and picture leases from it;
    /// the session takes the work and result buffers from it as a job needs them.
    #[must_use]
    pub fn pool(&self) -> &styx_frame::FramePool<usize> {
        self.stream.pool()
    }

    /// The job stream's name, as the device answered it.
    #[must_use]
    pub fn stream_name(&self) -> &str {
        self.stream.name()
    }

    /// Jobs not posted since the session opened, for want of a free buffer or a queue slot.
    #[must_use]
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// Completions this session's reader was too late for (`JobStream::skipped`). A job stream never
    /// replaces an unread completion, so this stays 0.
    #[must_use]
    pub fn skipped(&self) -> u64 {
        self.stream.skipped()
    }

    /// Waits for the completion of the job `identity` whose result is `result`, reads its
    /// record and releases the frame.
    fn completion(&mut self, identity: u64, result: u32) -> Result<u32, CodecError> {
        let frame = self
            .stream
            .wait(self.timeout)
            .map_err(|e| CodecError::Failed(format!("the job stream: {e}")))?
            .ok_or_else(|| {
                CodecError::Failed(format!("no completion within {:?}", self.timeout))
            })?;
        let outcome = read_completion(&self.stream, &frame, identity, result);
        self.stream
            .release(frame)
            .map_err(|e| CodecError::Failed(format!("releasing the completion: {e}")))?;
        outcome
    }
}

/// The name the device's `job_stream` answers.
fn job_stream_name(client: &Client, device: &DeviceEntry) -> Result<String, CodecError> {
    let mut block = client
        .open::<dyn CodecBlock>(&device.name)
        .map_err(|e| CodecError::Unavailable(format!("{}: {e}", device.name)))?;
    let mut name = [0u8; MAX_STREAM_NAME];
    let len = block
        .job_stream(&mut name)
        .map_err(|e| CodecError::Unavailable(format!("{}: job_stream: {e:?}", device.name)))?;
    std::str::from_utf8(&name[..len])
        .map(str::to_owned)
        .map_err(|_| {
            CodecError::Unavailable(format!("{}: job stream name is not UTF-8", device.name))
        })
}

/// What a completion frame says about the job `identity` whose result is `result`.
fn read_completion(
    stream: &JobStream<usize>,
    frame: &HeldFrame,
    identity: u64,
    result: u32,
) -> Result<u32, CodecError> {
    if frame.identity() != identity || frame.buffer() != result {
        return Err(CodecError::Failed(format!(
            "a completion for job {} on buffer {}, while job {identity} is waited for",
            frame.identity(),
            frame.buffer()
        )));
    }
    let bytes = stream.bytes(frame);
    let record = bytes
        .get(..COMPLETION_BYTES)
        .ok_or_else(|| CodecError::Failed("the completion has no record".into()))?;
    let record = Completion::decode(&mut Reader::new(record))
        .map_err(|_| CodecError::Failed("the completion record is malformed".into()))?;
    match record.status {
        JobStatus::Done => {
            let count = record.count as usize;
            if frame.len() != DATA_OFFSET + count {
                return Err(CodecError::Failed(format!(
                    "job {identity} completed with {} bytes for {count} of read-back",
                    frame.len()
                )));
            }
            Ok(record.count)
        }
        JobStatus::Refused => Err(CodecError::Rejected(format!(
            "the block refused job {identity}"
        ))),
        JobStatus::Failed => Err(CodecError::Failed(format!(
            "the block failed job {identity}"
        ))),
    }
}

/// The format a buffer the session takes for a job's own use is marked with. It means nothing
/// to the block: the lease only keeps the buffer from anyone else for the job's length.
fn reservation_format() -> Result<MediaFormat, CodecError> {
    let resolution = Resolution::new(1, 1)
        .ok_or_else(|| CodecError::Failed("a 1x1 resolution is not one".into()))?;
    Ok(MediaFormat::with_defaults(FourCc::HEVC, resolution))
}

impl Session for LiveSession {
    fn run(&mut self, work: &[u8]) -> Result<u32, CodecError> {
        let parsed = Work::parse(work)
            .ok_or_else(|| CodecError::Rejected("the job's work is not work".into()))?;
        // The buffers the job names: its work's references, one bit each.
        let mut named = 0u64;
        for reference in parsed.refs() {
            let buffer = reference.buffer as usize;
            if buffer >= MAX_NAMED {
                return Err(CodecError::Rejected(format!(
                    "buffer {buffer} is out of range"
                )));
            }
            named |= 1 << buffer;
        }
        let pool = self.stream.pool();
        let count = pool.buffers();
        let free = |index: usize| {
            index < count.min(MAX_NAMED) && named & (1 << index) == 0 && pool.is_free(index)
        };
        let Some(work_index) = (0..count).find(|&index| free(index)) else {
            self.refused += 1;
            return Err(CodecError::Failed(
                "refused: no free buffer for the job's work".into(),
            ));
        };
        let Some(result_index) = (0..count).find(|&index| index != work_index && free(index))
        else {
            self.refused += 1;
            return Err(CodecError::Failed(
                "refused: no free buffer for the job's result".into(),
            ));
        };

        let identity = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let format = reservation_format()?;

        // The work, written by the consumer into its own buffer. The lease holds the buffer for
        // the job; it is given up once the completion has been read.
        let work_layout = Layouts::new(&[PlaneLayout::new(0, work.len(), work.len())])
            .ok_or_else(|| CodecError::Rejected("the work is empty".into()))?;
        let mut work_lease = pool
            .acquire_at(
                work_index,
                FrameEntry::new(identity, 0, format, work_layout),
            )
            .map_err(|e| CodecError::Failed(format!("work buffer {work_index}: {e:?}")))?;
        work_lease
            .write()
            .ok_or_else(|| CodecError::Failed("the work buffer is not writable".into()))?
            .bytes()
            .get_mut(..work.len())
            .ok_or_else(|| CodecError::Failed("the work is larger than its buffer".into()))?
            .copy_from_slice(work);

        let result_layout = Layouts::new(&[PlaneLayout::new(0, DATA_OFFSET, DATA_OFFSET)])
            .ok_or_else(|| CodecError::Failed("the result layout".into()))?;
        let _result = pool
            .acquire_at(
                result_index,
                FrameEntry::new(identity, 0, format, result_layout),
            )
            .map_err(|e| CodecError::Failed(format!("result buffer {result_index}: {e:?}")))?;

        let job = Job {
            identity,
            work: work_index as u32,
            work_bytes: u32::try_from(work.len())
                .map_err(|_| CodecError::Rejected("the work is too long for a job".into()))?,
            result: result_index as u32,
        };
        let mut encoded = [0u8; JOB_BYTES];
        job.encode(&mut Writer::new(&mut encoded))
            .map_err(|_| CodecError::Failed("the job does not encode".into()))?;
        let posted = self
            .stream
            .post(&encoded)
            .map_err(|e| CodecError::Failed(format!("posting job {identity}: {e}")))?;
        if !posted {
            self.refused += 1;
            return Err(CodecError::Failed("refused: the job queue is full".into()));
        }
        self.completion(identity, result_index as u32)
    }
}
