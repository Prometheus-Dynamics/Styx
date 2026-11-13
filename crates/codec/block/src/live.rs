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
//! `drivers/codec-jobs` for the block side). The mapping of a stream's buffers is the same as
//! `styx_lemnos::open_frames`, which is read-only; a writable job stream needs this module's
//! `Mapping`, which should move into `styx-lemnos` as an `open_job_stream` next to it.

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use lemnos_contract_codec_block::work::Work;
use lemnos_contract_codec_block::{
    COMPLETION_BYTES, CodecBlock, Completion, DATA_OFFSET, JOB_BYTES, Job, JobStatus,
    MAX_STREAM_NAME,
};
use lemnos_core::{Decode, Encode, Reader, Writer};
use lemnos_linux_stream::LinuxStreams;
use lemnos_wire::ring::{ByteMemory, WordMemory};
use styx_codec_api::{CodecError, FourCc};
use styx_format::{MediaFormat, Resolution};
use styx_frame::{FdMemory, FrameEntry, FramePool, Layouts, PlaneLayout};
use styx_lemnos::{
    Client, DeviceEntry, FrameStream, HeldFrame, StreamPlatform, each_memories, pool_over,
    whole_memories,
};

use crate::decoder::Session;

/// The tag of a codec block device.
pub const CODEC_TAG: &str = "codec.block/1";

/// Most buffers one job can name. The contract's job stream holds at most this many.
const MAX_NAMED: usize = 64;

/// A `codec.block/1` device's job stream as a [`Session`], with the pool its buffers make.
pub struct LiveSession {
    stream: FrameStream,
    pool: FramePool<usize>,
    /// Identity of the next job. Only the completion with this number is the job's.
    sequence: u64,
    timeout: Duration,
}

impl std::fmt::Debug for LiveSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveSession")
            .field("stream", &self.stream.name())
            .field("buffers", &self.pool.buffers())
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
        let platform = Arc::new(Mapping::default());
        let stream = client
            .open_stream_writable(device.index, &name, platform.clone())
            .map_err(|e| CodecError::Unavailable(format!("job stream {name}: {e}")))?;
        let buffers = stream.layout().buffers as usize;
        let buffer_bytes = stream.layout().buffer_bytes as usize;
        if (stream.layout().job_bytes as usize) < JOB_BYTES {
            return Err(CodecError::Unavailable(format!(
                "job stream {name}: jobs are {} bytes, the contract's are {JOB_BYTES}",
                stream.layout().job_bytes
            )));
        }
        let mapped = std::mem::take(&mut *platform.lock());
        let memories = match &mapped.whole {
            Some(whole) => whole_memories(whole, buffers, buffer_bytes),
            None => each_memories(&mapped.each, buffer_bytes),
        }
        .map_err(|e| CodecError::Unavailable(format!("job stream {name}: {e}")))?;
        let pool = pool_over(memories, buffer_bytes)
            .map_err(|e| CodecError::Unavailable(format!("job stream {name}: {e}")))?;
        Ok(Self {
            stream,
            pool,
            sequence: 0,
            timeout,
        })
    }

    /// The pool over the job stream's buffers. Take the bitstream and picture leases from it;
    /// the session takes the work and result buffers from it as a job needs them.
    #[must_use]
    pub fn pool(&self) -> &FramePool<usize> {
        &self.pool
    }

    /// The job stream's name, as the device answered it.
    #[must_use]
    pub fn stream_name(&self) -> &str {
        self.stream.name()
    }

    /// Waits for the completion of the job `identity` whose result is `result`, reads its
    /// record and releases the frame.
    fn completion(&mut self, identity: u64, result: u32) -> Result<u32, CodecError> {
        let frame = self
            .stream
            .next_frame(self.timeout)
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
    stream: &FrameStream,
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
        let count = self.pool.buffers();
        let free = |index: usize| {
            index < count.min(MAX_NAMED) && named & (1 << index) == 0 && self.pool.is_free(index)
        };
        let work_index = (0..count)
            .find(|&index| free(index))
            .ok_or_else(|| CodecError::Failed("no free buffer for the job's work".into()))?;
        let result_index = (0..count)
            .find(|&index| index != work_index && free(index))
            .ok_or_else(|| CodecError::Failed("no free buffer for the job's result".into()))?;

        let identity = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let format = reservation_format()?;

        // The work, written by the consumer into its own buffer. The lease holds the buffer for
        // the job; it is given up once the completion has been read.
        let work_layout = Layouts::new(&[PlaneLayout::new(0, work.len(), work.len())])
            .ok_or_else(|| CodecError::Rejected("the work is empty".into()))?;
        let mut work_lease = self
            .pool
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
        let _result = self
            .pool
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
            .post_job(&encoded)
            .map_err(|e| CodecError::Failed(format!("posting job {identity}: {e}")))?;
        if !posted {
            return Err(CodecError::Failed("the job queue is full".into()));
        }
        self.completion(identity, result_index as u32)
    }
}

/// The platform a job stream is opened with: the descriptors go to [`LinuxStreams`] as they
/// would for a reader, and each buffer's descriptor is also mapped here as an [`FdMemory`], so
/// the frame pool and the stream read and write the same memory.
#[derive(Debug)]
struct Mapping {
    inner: LinuxStreams,
    mapped: Mutex<Mapped>,
}

impl Default for Mapping {
    fn default() -> Self {
        Self {
            inner: LinuxStreams,
            mapped: Mutex::new(Mapped::default()),
        }
    }
}

#[derive(Debug, Default)]
struct Mapped {
    /// One memfd for every buffer, when the stream is shared memory.
    whole: Option<Arc<FdMemory>>,
    /// One dma-buf per buffer, when the stream is one.
    each: Vec<Arc<FdMemory>>,
}

impl Mapping {
    fn lock(&self) -> MutexGuard<'_, Mapped> {
        self.mapped.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl StreamPlatform for Mapping {
    fn recv_with_descriptors(
        &self,
        socket: BorrowedFd<'_>,
        buf: &mut [u8],
        fds: &mut Vec<OwnedFd>,
    ) -> io::Result<usize> {
        self.inner.recv_with_descriptors(socket, buf, fds)
    }

    fn map_ring(&self, fd: BorrowedFd<'_>, bytes: usize) -> io::Result<Arc<dyn WordMemory>> {
        self.inner.map_ring(fd, bytes)
    }

    fn map_buffers(
        &self,
        fd: BorrowedFd<'_>,
        bytes: usize,
        writable: bool,
    ) -> io::Result<Box<dyn ByteMemory>> {
        let mine = FdMemory::memfd(fd.try_clone_to_owned()?, bytes, writable)?;
        self.lock().whole = Some(Arc::new(mine));
        self.inner.map_buffers(fd, bytes, writable)
    }

    fn map_buffer(
        &self,
        fd: BorrowedFd<'_>,
        bytes: usize,
        writable: bool,
    ) -> io::Result<Box<dyn ByteMemory>> {
        let mine = FdMemory::dmabuf(fd.try_clone_to_owned()?, bytes, writable)?;
        self.lock().each.push(Arc::new(mine));
        self.inner.map_buffer(fd, bytes, writable)
    }

    fn wait_doorbell(&self, fd: BorrowedFd<'_>, timeout: Duration) -> io::Result<bool> {
        self.inner.wait_doorbell(fd, timeout)
    }
}
