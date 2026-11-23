//! Replays a `styxrec` recording as a camera.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use styx_controls::FrameMetadata;
use styx_frame::{AcquireError, BufferSize, FrameEntry, FrameLease, FramePool};

use crate::channel::{Channel, ChannelId, Sample};
use crate::crc::{Crc32, crc32};
use crate::error::RecordError;
use crate::format::{
    MAX_CHANNEL_RECORD, MAX_CHANNELS, MAX_FRAME_HEADER, RecordingHeader, SAMPLE_HEAD, TAG_CHANNEL,
    TAG_END, TAG_FRAME, TAG_SAMPLE, decode_channel, decode_frame_header, decode_sample_head,
    read_header,
};

/// Frames a consumer may hold at once. A consumer that holds more has frames dropped, as a
/// camera drops when its consumer is slow.
pub const REPLAY_BUFFERS: usize = 4;
/// Frames [`Replay::next_item`] holds for its lookahead: the frame it will hand out next and the
/// one after it. The pool has [`REPLAY_BUFFERS`] plus these; [`Replay::next_frame`] holds none,
/// so its consumer may hold them all.
pub const LOOKAHEAD_FRAMES: usize = 2;
/// Most sample bytes [`Replay::next_item`] queues while it reads ahead to the next frame.
pub const LOOKAHEAD_SAMPLE_BYTES: usize = 8 << 20;
/// Most samples [`Replay::next_item`] queues while it reads ahead to the next frame.
pub const LOOKAHEAD_SAMPLES: usize = 16 * 1024;

/// A sample read ahead: its channel, time, and where its bytes are in the queue's arena.
#[derive(Debug, Clone, Copy)]
struct Queued {
    channel: ChannelId,
    timestamp_ns: u64,
    start: usize,
    len: usize,
}

/// What [`Replay::next_item`] hands out: a frame, or a sample of a data channel.
#[derive(Debug)]
pub enum Item<'a> {
    /// A frame, as [`Replay::next_frame`] gives it.
    Frame(FrameLease<FrameMetadata>),
    /// A sample, its bytes borrowed until the next call.
    Sample(Sample<'a>),
}

/// One record read: what it was.
enum Record {
    /// The recording ended (cleanly or cut short).
    End,
    /// A frame, in a lease from the pool.
    Frame(FrameLease<FrameMetadata>),
    /// A frame with no free buffer, skipped and counted.
    Dropped,
    /// A channel declaration, added to the list.
    Channel,
    /// A sample, queued.
    Sample,
}

/// Reads a recording and yields its frames as leases from its own pool, and the samples of
/// its data channels.
///
/// Each lease carries the frame's identity, timestamp, format, layout and metadata as recorded,
/// and the frame's bytes. A frame whose buffer is not free (every buffer is held) is skipped and
/// counted in [`Replay::dropped`]. A recording cut short ends at its last whole frame, with
/// [`Replay::truncated`] set.
///
/// [`Replay::next_frame`] gives the frames only. [`Replay::next_item`] gives frames and
/// samples in timestamp order (see there). Reading allocates nothing once the sample queue has
/// grown to the most it holds at once; declarations, which come once per channel, allocate.
#[derive(Debug)]
pub struct Replay<R: Read> {
    source: R,
    header: RecordingHeader,
    version: u16,
    pool: FramePool<FrameMetadata>,
    frame_header: Vec<u8>,
    channels: Vec<Channel>,
    /// Frames read ahead by `next_item`, oldest first, not handed out yet (at most
    /// [`LOOKAHEAD_FRAMES`]).
    ahead: VecDeque<FrameLease<FrameMetadata>>,
    /// Samples read ahead, in the recording's order, and the bytes they point into.
    queue: VecDeque<Queued>,
    /// The queued samples' bytes in total.
    queued_bytes: usize,
    arena: Vec<u8>,
    last_frame_ns: Option<u64>,
    /// A record tag read by `new` while it read the leading declarations.
    peeked: Option<u8>,
    delivered: u64,
    dropped: u64,
    samples: u64,
    late_samples: u64,
    ended: bool,
    truncated: bool,
    end_marker: bool,
}

impl Replay<BufReader<File>> {
    /// Opens the recording at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RecordError> {
        Self::new(BufReader::with_capacity(1 << 20, File::open(path)?))
    }
}

impl<R: Read> Replay<R> {
    /// Reads the header of a recording from `source`, and the channel declarations that come
    /// straight after it, and makes the pool its frames come from.
    pub fn new(mut source: R) -> Result<Self, RecordError> {
        let (header, version) = read_header(&mut source)?;
        let payload =
            usize::try_from(header.payload).map_err(|_| RecordError::TooLarge("payload"))?;
        let pool = FramePool::host(
            REPLAY_BUFFERS + LOOKAHEAD_FRAMES,
            BufferSize::new(payload, 0),
        )
        .map_err(|e| RecordError::Frame(e.to_string()))?;
        let mut replay = Self {
            source,
            header,
            version,
            pool,
            frame_header: Vec::with_capacity(256),
            channels: Vec::new(),
            ahead: VecDeque::with_capacity(LOOKAHEAD_FRAMES),
            queue: VecDeque::with_capacity(256),
            queued_bytes: 0,
            arena: Vec::with_capacity(64 * 1024),
            last_frame_ns: None,
            peeked: None,
            delivered: 0,
            dropped: 0,
            samples: 0,
            late_samples: 0,
            ended: false,
            truncated: false,
            end_marker: false,
        };
        if replay.version >= 2 {
            replay.read_declarations()?;
        }
        Ok(replay)
    }

    /// Reads the channel declarations at the start of the records, and keeps the tag of the
    /// first record that is not one.
    fn read_declarations(&mut self) -> Result<(), RecordError> {
        loop {
            let mut tag = [0u8; 1];
            if !read_full(&mut self.source, &mut tag)? {
                self.ended = true;
                return Ok(());
            }
            if tag[0] != TAG_CHANNEL {
                self.peeked = Some(tag[0]);
                return Ok(());
            }
            self.read_channel()?;
            if self.ended {
                return Ok(());
            }
        }
    }

    /// The recording's header.
    pub fn header(&self) -> &RecordingHeader {
        &self.header
    }

    /// The recording's format version (1 or 2).
    pub fn version(&self) -> u16 {
        self.version
    }

    /// The channels declared so far, in order (a channel's [`ChannelId`] is its index). The
    /// declarations at the start of the recording are read when it opens; one declared later is
    /// known from its declaration on.
    pub fn channels(&self) -> &[Channel] {
        &self.channels
    }

    /// The declared channel named `name`.
    pub fn channel_named(&self, name: &str) -> Option<&Channel> {
        self.channels.iter().find(|c| c.spec.name == name)
    }

    /// The pool the frames are leased from (its buffers, statistics and sizes).
    pub fn pool(&self) -> &FramePool<FrameMetadata> {
        &self.pool
    }

    /// Frames handed out so far.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// Frames skipped because no buffer was free.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Samples handed out so far by [`Replay::next_item`].
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Samples handed out after a frame newer than them (see [`Sample::late`]).
    pub fn late_samples(&self) -> u64 {
        self.late_samples
    }

    /// Whether the recording's end marker has been read: it was finished
    /// ([`crate::Recorder::finish`]). A recording that stops between records without it was
    /// not finished but lost nothing; one cut inside a record is [`Replay::truncated`].
    pub fn end_marker(&self) -> bool {
        self.end_marker
    }

    /// Whether the recording ended inside a record (it was cut short while being written).
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// The next frame, or `None` at the end of the recording. Samples are read past (and any
    /// [`Replay::next_item`] had queued are passed over). A frame whose buffer is not free is
    /// skipped and counted in [`Replay::dropped`]. A record that fails its checks is an error:
    /// the recording is refused from there on.
    pub fn next_frame(&mut self) -> Result<Option<FrameLease<FrameMetadata>>, RecordError> {
        self.clear_queue();
        if let Some(frame) = self.ahead.pop_front() {
            return Ok(Some(self.hand_out(frame)));
        }
        loop {
            match self.read_record()? {
                Record::End => return Ok(None),
                Record::Frame(frame) => return Ok(Some(self.hand_out(frame))),
                Record::Dropped | Record::Channel => {}
                Record::Sample => {
                    self.clear_queue();
                }
            }
        }
    }

    /// The next frame or sample, or `None` at the end of the recording.
    ///
    /// Items come out in timestamp order, with one frame of lookahead: a frame is handed out
    /// once the frame after it has been read (or the recording has ended), and the samples read
    /// meanwhile are queued. Before a frame come every queued sample at or before its time (a
    /// sample at a frame's exact time comes before it); the rest wait for the next frame. So a
    /// sample may reach the recording up to one frame late, and a frame up to a frame interval
    /// after its own time, and both still come out in order.
    ///
    /// A sample older than a frame already handed out is late: it reached the recording more
    /// than a frame after its time. It comes out in the recording's order, marked
    /// [`Sample::late`] and counted in [`Replay::late_samples`]; it is never dropped. Samples
    /// keep the recording's order among themselves.
    ///
    /// The lookahead holds [`LOOKAHEAD_FRAMES`] frames of its own (the pool has them on top of
    /// [`REPLAY_BUFFERS`]) and queues up to [`LOOKAHEAD_SAMPLE_BYTES`] / [`LOOKAHEAD_SAMPLES`]
    /// of samples; when the queue is full before the next frame is read (a recording of samples
    /// only, or a long gap between frames), samples come out in the recording's order without
    /// waiting. A sample's bytes are borrowed until the next call. Nothing is allocated once the
    /// queue has grown to the most it holds.
    ///
    /// The samples of a channel between frame n-1 and frame n are the ones that come out
    /// between them:
    ///
    /// ```no_run
    /// # fn main() -> Result<(), styx_record::RecordError> {
    /// use styx_record::{Item, Replay};
    /// let mut replay = Replay::open("run.styxrec")?;
    /// let imu = replay.channel_named("/imu").map(|c| c.id);
    /// let mut since_last_frame: Vec<(u64, Vec<u8>)> = Vec::new();
    /// while let Some(item) = replay.next_item()? {
    ///     match item {
    ///         Item::Sample(s) if Some(s.channel) == imu && !s.late => {
    ///             since_last_frame.push((s.timestamp_ns, s.data.to_vec()))
    ///         }
    ///         Item::Frame(frame) => {
    ///             // `since_last_frame`: the IMU samples after the previous frame, up to this
    ///             // one's time. The last of them is the nearest at or before the frame;
    ///             // its gap is `frame.timestamp_ns() - t`.
    ///             since_last_frame.clear();
    ///         }
    ///         Item::Sample(_) => {}
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn next_item(&mut self) -> Result<Option<Item<'_>>, RecordError> {
        loop {
            // A queued sample at or before the oldest frame held comes out now: no frame read
            // later is older than that frame, so nothing can come before it.
            if let Some(frame) = self.ahead.front() {
                let ts = frame.timestamp_ns();
                if let Some(i) = self.queue.iter().position(|q| q.timestamp_ns <= ts) {
                    return Ok(Some(self.take_sample(i)));
                }
            }
            let ready = self.ended || self.ahead.len() >= LOOKAHEAD_FRAMES || self.queue_full();
            if !ready {
                match self.read_record()? {
                    Record::Frame(frame) => self.ahead.push_back(frame),
                    Record::End | Record::Dropped | Record::Channel | Record::Sample => {}
                }
                continue;
            }
            if let Some(frame) = self.ahead.pop_front() {
                return Ok(Some(Item::Frame(self.hand_out(frame))));
            }
            if !self.queue.is_empty() {
                return Ok(Some(self.take_sample(0)));
            }
            return Ok(None);
        }
    }

    fn queue_full(&self) -> bool {
        self.queue.len() >= LOOKAHEAD_SAMPLES || self.queued_bytes >= LOOKAHEAD_SAMPLE_BYTES
    }

    fn clear_queue(&mut self) {
        self.queue.clear();
        self.queued_bytes = 0;
        self.arena.clear();
    }

    fn hand_out(&mut self, frame: FrameLease<FrameMetadata>) -> FrameLease<FrameMetadata> {
        self.delivered += 1;
        self.last_frame_ns = Some(frame.timestamp_ns());
        frame
    }

    /// Takes queued sample `i` out of the queue and hands it out. Its bytes stay in the arena
    /// until the next sample is queued.
    fn take_sample(&mut self, i: usize) -> Item<'_> {
        // In the recording's order this is the front: O(1).
        let Some(q) = self.queue.remove(i) else {
            unreachable!("a queued sample's index")
        };
        self.queued_bytes -= q.len;
        let late = self.last_frame_ns.is_some_and(|t| q.timestamp_ns <= t);
        self.samples += 1;
        if late {
            self.late_samples += 1;
        }
        Item::Sample(Sample {
            channel: q.channel,
            timestamp_ns: q.timestamp_ns,
            data: &self.arena[q.start..q.start + q.len],
            late,
        })
    }

    /// Room for `len` more bytes at the arena's end, reusing the space of samples already
    /// handed out: the arena is emptied when the queue is, and otherwise packed (the queued
    /// samples moved to its front, in order) when it would have to grow. After a pack the arena
    /// has room for at least as many bytes again as are queued, so a queue that stays full (a
    /// recording of samples only) packs once per queue's worth of samples, not once per sample:
    /// the arena grows to at most twice what the queue holds, then stops.
    fn make_room(&mut self, len: usize) {
        if self.queue.is_empty() {
            self.arena.clear();
        } else if self.arena.len() + len > self.arena.capacity() {
            let mut end = 0;
            for q in &mut self.queue {
                self.arena.copy_within(q.start..q.start + q.len, end);
                q.start = end;
                end += q.len;
            }
            self.arena.truncate(end);
            self.arena.reserve(end + len);
        }
    }

    /// Reads the next record.
    fn read_record(&mut self) -> Result<Record, RecordError> {
        if self.ended {
            return Ok(Record::End);
        }
        let tag = match self.peeked.take() {
            Some(tag) => tag,
            None => {
                let mut tag = [0u8; 1];
                if !read_full(&mut self.source, &mut tag)? {
                    // No end marker: the file simply stops between records.
                    self.ended = true;
                    return Ok(Record::End);
                }
                tag[0]
            }
        };
        match tag {
            TAG_END => {
                self.ended = true;
                self.end_marker = true;
                Ok(Record::End)
            }
            TAG_FRAME => self.read_frame(),
            TAG_CHANNEL if self.version >= 2 => self.read_channel(),
            TAG_SAMPLE if self.version >= 2 => self.read_sample(),
            _ => Err(RecordError::Corrupt("unknown record tag")),
        }
    }

    fn read_frame(&mut self) -> Result<Record, RecordError> {
        let Some((entry, meta)) = self.read_frame_header()? else {
            return Ok(Record::End);
        };
        let span = entry
            .layouts
            .span()
            .ok_or(RecordError::Corrupt("plane layout overflows"))?;
        match self.pool.acquire(entry) {
            Ok(mut lease) => {
                let filled = {
                    let mut planes = lease
                        .write()
                        .ok_or_else(|| RecordError::Frame("buffer is not writable".into()))?;
                    let buf = planes.bytes().get_mut(..span).ok_or_else(|| {
                        RecordError::Frame("buffer smaller than the frame".into())
                    })?;
                    read_full(&mut self.source, buf)?
                };
                if !filled {
                    return Ok(self.cut());
                }
                *lease
                    .meta_mut()
                    .ok_or_else(|| RecordError::Frame("buffer is shared".into()))? = meta;
                Ok(Record::Frame(lease))
            }
            Err(AcquireError::Exhausted | AcquireError::Held { .. }) => {
                self.dropped += 1;
                let skipped = io::copy(&mut (&mut self.source).take(span as u64), &mut io::sink())?;
                if skipped < span as u64 {
                    return Ok(self.cut());
                }
                Ok(Record::Dropped)
            }
            Err(e) => Err(RecordError::Frame(e.to_string())),
        }
    }

    fn read_channel(&mut self) -> Result<Record, RecordError> {
        let mut len = [0u8; 4];
        if !read_full(&mut self.source, &mut len)? {
            return Ok(self.cut());
        }
        let len = u32::from_le_bytes(len) as usize;
        if len == 0 || len > MAX_CHANNEL_RECORD {
            return Err(RecordError::TooLarge("channel record"));
        }
        self.frame_header.clear();
        self.frame_header.resize(len, 0);
        let mut crc = [0u8; 4];
        if !read_full(&mut self.source, &mut self.frame_header)?
            || !read_full(&mut self.source, &mut crc)?
        {
            return Ok(self.cut());
        }
        if u32::from_le_bytes(crc) != crc32(&self.frame_header) {
            return Err(RecordError::Corrupt("channel checksum"));
        }
        let channel = decode_channel(&self.frame_header)?;
        if usize::from(channel.id.0) != self.channels.len() {
            return Err(RecordError::Corrupt("channel declared out of order"));
        }
        if self.channels.len() >= MAX_CHANNELS {
            return Err(RecordError::TooLarge("channels"));
        }
        if self.channel_named(&channel.spec.name).is_some() {
            return Err(RecordError::Corrupt("channel name declared twice"));
        }
        self.channels.push(channel);
        Ok(Record::Channel)
    }

    fn read_sample(&mut self) -> Result<Record, RecordError> {
        let mut head = [0u8; SAMPLE_HEAD];
        if !read_full(&mut self.source, &mut head)? {
            return Ok(self.cut());
        }
        let (channel, timestamp_ns, len) = decode_sample_head(&head)?;
        if usize::from(channel.0) >= self.channels.len() {
            return Err(RecordError::Corrupt("sample of a channel not declared"));
        }
        self.make_room(len);
        let start = self.arena.len();
        // Within the arena's capacity once it has grown to the most the queue holds.
        self.arena.resize(start + len, 0);
        let mut crc = [0u8; 4];
        if !read_full(&mut self.source, &mut self.arena[start..])?
            || !read_full(&mut self.source, &mut crc)?
        {
            self.arena.truncate(start);
            return Ok(self.cut());
        }
        let expected = Crc32::new()
            .update(&head)
            .update(&self.arena[start..])
            .value();
        if u32::from_le_bytes(crc) != expected {
            self.arena.truncate(start);
            return Err(RecordError::Corrupt("sample checksum"));
        }
        self.queued_bytes += len;
        self.queue.push_back(Queued {
            channel,
            timestamp_ns,
            start,
            len,
        });
        Ok(Record::Sample)
    }

    /// The frame record's header after its tag: length, checksummed bytes, checksum. `None` when
    /// the recording ends inside it.
    fn read_frame_header(&mut self) -> Result<Option<(FrameEntry, FrameMetadata)>, RecordError> {
        let mut len = [0u8; 4];
        if !read_full(&mut self.source, &mut len)? {
            return Ok(self.cut());
        }
        let len = u32::from_le_bytes(len) as usize;
        if len == 0 || len > MAX_FRAME_HEADER {
            return Err(RecordError::TooLarge("frame header"));
        }
        self.frame_header.clear();
        self.frame_header.resize(len, 0);
        let mut crc = [0u8; 4];
        if !read_full(&mut self.source, &mut self.frame_header)?
            || !read_full(&mut self.source, &mut crc)?
        {
            return Ok(self.cut());
        }
        if u32::from_le_bytes(crc) != crc32(&self.frame_header) {
            return Err(RecordError::Corrupt("frame checksum"));
        }
        decode_frame_header(&self.frame_header, self.header.payload).map(Some)
    }

    /// The recording ends here, cut short.
    fn cut<T: CutShort>(&mut self) -> T {
        self.ended = true;
        self.truncated = true;
        T::cut()
    }
}

/// What a reader returns when the recording is cut short inside a record.
trait CutShort {
    fn cut() -> Self;
}

impl<T> CutShort for Option<T> {
    fn cut() -> Self {
        None
    }
}

impl CutShort for Record {
    fn cut() -> Self {
        Record::End
    }
}

/// Fills `buf` from `r`. `Ok(false)` when the data ends first (the bytes read are in `buf`'s
/// prefix, and the caller treats the record as cut short).
pub(crate) fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut done = 0;
    while done < buf.len() {
        match r.read(&mut buf[done..]) {
            Ok(0) => return Ok(false),
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}
