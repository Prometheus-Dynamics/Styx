//! MCAP recordings written from a background thread ([`StreamRecorder::mcap`]): options, the
//! bounded queue between the recording thread and the writer, statistics, and the host's own
//! channels, metadata and attachments.
//!
//! A frame is encoded on the caller's thread (its pixels copied into the message, so no lease
//! is held while the disk catches up) and queued. When the queue holds
//! [`RecorderOptions::queue_bytes`], further frames and host messages are dropped and counted
//! ([`RecorderStats`]) instead of blocking the caller; channels, metadata and attachments are
//! never dropped.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::ReplayError;
use super::mcap_format::{EncodedMsg, McapRecorder};

/// Chunk compression of an MCAP recording. `Lz4` needs feature `replay-mcap-lz4`, `Zstd`
/// feature `replay-mcap-zstd` (both build a C library); without them the recording is refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RecordCompression {
    /// Uncompressed chunks.
    #[default]
    None,
    /// LZ4 frames: fast, modest ratio.
    Lz4,
    /// Zstandard at `level` (1: fastest; 3: zstd's default).
    Zstd { level: u32 },
}

/// How [`StreamRecorder::mcap`] writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecorderOptions {
    /// Chunk compression (default none).
    pub compression: RecordCompression,
    /// Extra threads compressing zstd chunks (default 0: the writer thread compresses).
    pub compression_threads: u32,
    /// Chunk size in bytes (`None`, default: the MCAP library's).
    pub chunk_size: Option<u64>,
    /// Bytes of encoded messages waiting for the disk before frames and host messages are
    /// dropped (default 64 MiB). 0: no writer thread; every call writes before it returns.
    pub queue_bytes: usize,
    /// Clock samples on `/styx/clock` (monotonic, boot time and real time read together): at
    /// the start and then every period (default 1 s), so timestamps on one clock can be put on
    /// another offline. `None`: none.
    pub clock_period: Option<Duration>,
    /// How 16-bit Bayer frames (`Frames::sensor_raw`) are stored (default as they are).
    pub raw_storage: super::RawStorage,
}

impl Default for RecorderOptions {
    fn default() -> Self {
        Self {
            compression: RecordCompression::None,
            compression_threads: 0,
            chunk_size: None,
            queue_bytes: 64 << 20,
            clock_period: Some(Duration::from_secs(1)),
            raw_storage: super::RawStorage::AsIs,
        }
    }
}

/// What a recorder wrote and dropped so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecorderStats {
    /// Frames written (each with its companions and metadata message).
    pub frames_written: u64,
    /// Frames dropped because the queue was full.
    pub frames_dropped: u64,
    /// Host messages ([`StreamRecorder::write`]) written.
    pub messages_written: u64,
    /// Host messages dropped because the queue was full.
    pub messages_dropped: u64,
    /// Message bytes written (before chunk compression).
    pub bytes_written: u64,
    /// Bytes waiting in the queue now, and the most there were.
    pub queued_bytes: u64,
    pub max_queued_bytes: u64,
    /// Time the writer spent writing frames (compression included): in total and the longest.
    pub write_time: Duration,
    pub max_write_time: Duration,
}

/// A host channel of a recording ([`StreamRecorder::add_channel`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChannelId(pub(super) u16);

/// The schema of a host channel: its name, encoding (`jsonschema`, `ros2msg`, `protobuf`...)
/// and definition. Messages without a schema (e.g. plain JSON) pass `None` instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelSchema {
    pub name: String,
    pub encoding: String,
    pub data: Vec<u8>,
}

/// Host channel ids start here, away from the recorder's own.
pub(super) const FIRST_HOST_CHANNEL: u16 = 0x8000;

/// One thing for the writer.
pub(super) enum Item {
    Frame {
        sequence: u32,
        msgs: Vec<EncodedMsg>,
    },
    Message {
        channel: u16,
        sequence: u32,
        log_time: u64,
        publish_time: u64,
        data: Vec<u8>,
    },
    Channel {
        id: u16,
        topic: String,
        schema: Option<ChannelSchema>,
        message_encoding: String,
        metadata: BTreeMap<String, String>,
    },
    Metadata {
        name: String,
        metadata: BTreeMap<String, String>,
    },
    Attachment {
        name: String,
        media_type: String,
        log_time: u64,
        data: Vec<u8>,
    },
}

impl Item {
    fn bytes(&self) -> usize {
        match self {
            Item::Frame { msgs, .. } => msgs.iter().map(|m| m.data.len()).sum(),
            Item::Message { data, .. } | Item::Attachment { data, .. } => data.len(),
            Item::Channel { .. } | Item::Metadata { .. } => 0,
        }
    }

    /// Frames and host messages give way when the queue is full.
    fn droppable(&self) -> bool {
        matches!(self, Item::Frame { .. } | Item::Message { .. })
    }
}

/// Applies `item` to `out`, counting it in `stats`.
pub(super) fn apply(
    out: &mut McapRecorder,
    item: Item,
    stats: &Counters,
) -> Result<(), ReplayError> {
    match item {
        Item::Frame { sequence, msgs } => {
            let t = Instant::now();
            out.write_encoded(sequence, &msgs)?;
            let took = t.elapsed().as_nanos() as u64;
            stats.write_ns.fetch_add(took, Ordering::Relaxed);
            stats.max_write_ns.fetch_max(took, Ordering::Relaxed);
            let bytes: usize = msgs.iter().map(|m| m.data.len()).sum();
            stats.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
            stats.frames.fetch_add(1, Ordering::Relaxed);
        }
        Item::Message {
            channel,
            sequence,
            log_time,
            publish_time,
            data,
        } => {
            out.write_host(channel, sequence, log_time, publish_time, &data)?;
            stats.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
            stats.messages.fetch_add(1, Ordering::Relaxed);
        }
        Item::Channel {
            id,
            topic,
            schema,
            message_encoding,
            metadata,
        } => out.add_host_channel(id, &topic, schema.as_ref(), &message_encoding, &metadata)?,
        Item::Metadata { name, metadata } => out.metadata(name, metadata)?,
        Item::Attachment {
            name,
            media_type,
            log_time,
            data,
        } => out.attach(name, media_type, log_time, data)?,
    }
    out.clock_if_due()
}

/// The counters behind [`RecorderStats`].
#[derive(Default)]
pub(super) struct Counters {
    frames: AtomicU64,
    frames_dropped: AtomicU64,
    messages: AtomicU64,
    messages_dropped: AtomicU64,
    bytes: AtomicU64,
    max_queued: AtomicU64,
    write_ns: AtomicU64,
    max_write_ns: AtomicU64,
}

impl Counters {
    pub(super) fn snapshot(&self, queued: u64) -> RecorderStats {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        RecorderStats {
            frames_written: get(&self.frames),
            frames_dropped: get(&self.frames_dropped),
            messages_written: get(&self.messages),
            messages_dropped: get(&self.messages_dropped),
            bytes_written: get(&self.bytes),
            queued_bytes: queued,
            max_queued_bytes: get(&self.max_queued),
            write_time: Duration::from_nanos(get(&self.write_ns)),
            max_write_time: Duration::from_nanos(get(&self.max_write_ns)),
        }
    }

    fn dropped(&self, item: &Item) {
        let counter = match item {
            Item::Frame { .. } => &self.frames_dropped,
            _ => &self.messages_dropped,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

struct State {
    items: VecDeque<Item>,
    bytes: usize,
    closed: bool,
    /// The writer's first error: the recording stops there.
    error: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    capacity: usize,
    counters: Counters,
}

/// The writer thread and its queue.
pub(super) struct Background {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Result<(), ReplayError>>>,
}

impl Background {
    /// Starts the writer for `out`, holding at most `capacity` queued bytes.
    pub(super) fn start(mut out: McapRecorder, capacity: usize) -> Result<Self, ReplayError> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                items: VecDeque::new(),
                bytes: 0,
                closed: false,
                error: None,
            }),
            changed: Condvar::new(),
            capacity,
            counters: Counters::default(),
        });
        let s = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("styx-recorder".into())
            .spawn(move || {
                let result = run(&mut out, &s);
                if let Err(e) = &result {
                    let mut st = s.state.lock().unwrap_or_else(|e| e.into_inner());
                    st.error = Some(e.to_string());
                    st.items.clear();
                    st.bytes = 0;
                }
                result.and_then(|()| out.finish())
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Queues `item`; a frame or host message is dropped (and counted) when the queue is full.
    pub(super) fn push(&self, item: Item) -> Result<(), ReplayError> {
        let bytes = item.bytes();
        let mut st = self.lock();
        if let Some(e) = &st.error {
            return Err(ReplayError::Mcap(format!("recorder writer: {e}")));
        }
        if item.droppable() && st.bytes + bytes > self.shared.capacity {
            self.shared.counters.dropped(&item);
            return Ok(());
        }
        st.bytes += bytes;
        self.shared
            .counters
            .max_queued
            .fetch_max(st.bytes as u64, Ordering::Relaxed);
        st.items.push_back(item);
        drop(st);
        self.shared.changed.notify_one();
        Ok(())
    }

    pub(super) fn stats(&self) -> RecorderStats {
        let queued = self.lock().bytes as u64;
        self.shared.counters.snapshot(queued)
    }

    /// Writes what is queued, completes the file and waits for the writer.
    pub(super) fn finish(mut self) -> Result<(), ReplayError> {
        self.close()
    }

    fn close(&mut self) -> Result<(), ReplayError> {
        self.lock().closed = true;
        self.shared.changed.notify_one();
        match self.thread.take().map(JoinHandle::join) {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(ReplayError::Mcap("recorder writer panicked".into())),
            None => Ok(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Background {
    /// A recorder dropped without `finish` still writes what it queued and completes the file.
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// The writer: takes items until closed and empty, sampling the clocks when due meanwhile.
fn run(out: &mut McapRecorder, shared: &Shared) -> Result<(), ReplayError> {
    loop {
        let item = {
            let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(item) = st.items.pop_front() {
                    st.bytes -= item.bytes();
                    break Some(item);
                }
                if st.closed {
                    break None;
                }
                let wait = out.clock_wait().unwrap_or(Duration::from_secs(3600));
                st = shared
                    .changed
                    .wait_timeout(st, wait)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
                if st.items.is_empty() && !st.closed {
                    drop(st);
                    out.clock_if_due()?;
                    st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                }
            }
        };
        match item {
            Some(item) => apply(out, item, &shared.counters)?,
            None => return Ok(()),
        }
    }
}
