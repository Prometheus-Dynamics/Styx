//! Replays a `styxrec` recording as a camera.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use styx_controls::FrameMetadata;
use styx_frame::{AcquireError, BufferSize, FrameEntry, FrameLease, FramePool};

use crate::crc::crc32;
use crate::error::RecordError;
use crate::format::{
    MAX_FRAME_HEADER, RecordingHeader, TAG_END, TAG_FRAME, decode_frame_header, read_header,
};

/// Buffers a replay has: frames it hands out at once. A consumer that holds more has frames
/// dropped, as a camera drops when its consumer is slow.
pub const REPLAY_BUFFERS: usize = 4;

/// Reads a recording and yields its frames as leases from its own pool.
///
/// Each lease carries the frame's identity, timestamp, format, layout and metadata as recorded,
/// and the frame's bytes. A frame whose buffer is not free (every buffer is held) is skipped and
/// counted in [`Replay::dropped`]. A recording cut short ends at its last whole frame, with
/// [`Replay::truncated`] set.
#[derive(Debug)]
pub struct Replay<R: Read> {
    source: R,
    header: RecordingHeader,
    pool: FramePool<FrameMetadata>,
    frame_header: Vec<u8>,
    delivered: u64,
    dropped: u64,
    ended: bool,
    truncated: bool,
}

impl Replay<BufReader<File>> {
    /// Opens the recording at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RecordError> {
        Self::new(BufReader::with_capacity(1 << 20, File::open(path)?))
    }
}

impl<R: Read> Replay<R> {
    /// Reads the header of a recording from `source` and makes the pool its frames come from.
    pub fn new(mut source: R) -> Result<Self, RecordError> {
        let header = read_header(&mut source)?;
        let payload =
            usize::try_from(header.payload).map_err(|_| RecordError::TooLarge("payload"))?;
        let pool = FramePool::host(REPLAY_BUFFERS, BufferSize::new(payload, 0))
            .map_err(|e| RecordError::Frame(e.to_string()))?;
        Ok(Self {
            source,
            header,
            pool,
            frame_header: Vec::with_capacity(256),
            delivered: 0,
            dropped: 0,
            ended: false,
            truncated: false,
        })
    }

    /// The recording's header.
    pub fn header(&self) -> &RecordingHeader {
        &self.header
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

    /// Whether the recording ended inside a record (it was cut short while being written).
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// The next frame, or `None` at the end of the recording. A frame whose buffer is not free
    /// is skipped and counted in [`Replay::dropped`]. A record that fails its checks is an error:
    /// the recording is refused from there on.
    pub fn next_frame(&mut self) -> Result<Option<FrameLease<FrameMetadata>>, RecordError> {
        loop {
            if self.ended {
                return Ok(None);
            }
            let mut tag = [0u8; 1];
            if !read_full(&mut self.source, &mut tag)? {
                // No end marker: the file simply stops between records.
                self.ended = true;
                return Ok(None);
            }
            match tag[0] {
                TAG_END => {
                    self.ended = true;
                    return Ok(None);
                }
                TAG_FRAME => {}
                _ => return Err(RecordError::Corrupt("unknown record tag")),
            }
            let Some((entry, meta)) = self.read_frame_header()? else {
                return Ok(None);
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
                    self.delivered += 1;
                    return Ok(Some(lease));
                }
                Err(AcquireError::Exhausted | AcquireError::Held { .. }) => {
                    self.dropped += 1;
                    let skipped =
                        io::copy(&mut (&mut self.source).take(span as u64), &mut io::sink())?;
                    if skipped < span as u64 {
                        return Ok(self.cut());
                    }
                }
                Err(e) => return Err(RecordError::Frame(e.to_string())),
            }
        }
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
    fn cut<T>(&mut self) -> Option<T> {
        self.ended = true;
        self.truncated = true;
        None
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
