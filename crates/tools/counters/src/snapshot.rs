//! The snapshot record: one camera's counters at one instant, as fixed-size little-endian bytes.
//!
//! The record is versioned and has no heap, so it can be written into a ring buffer and read by a
//! process that shares no code with the writer beyond this layout. Every integer is little-endian.
//! Text fields are a length byte followed by a fixed number of bytes (the rest are zero).
//!
//! Layout, version 1 (offsets in bytes, `SNAPSHOT_BYTES` in total):
//!
//! | offset | size | field |
//! |---:|---:|---|
//! | 0 | 4 | magic `STCN` |
//! | 4 | 2 | version (1) |
//! | 6 | 2 | total length (`SNAPSHOT_BYTES`) |
//! | 8 | 8 | `seq`: the publisher's snapshot number |
//! | 16 | 8 | `taken_ns`: `CLOCK_MONOTONIC` when taken |
//! | 24 | 65 | `camera`: text, 64 bytes |
//! | 89 | 1 | `owner` code (0 none, 1 native, 2 libcamera, 3 v4l2) |
//! | 90 | 97 | `owner_reason`: text, 96 bytes |
//! | 187 | 1 | `state` code (0 absent, 1 streaming, 2 retrying, 3 backoff, 4 stopped, 5 off) |
//! | 188 | 1 | `health` code (0 unknown, 1 healthy, 2 degraded, 3 failed) |
//! | 189 | 65 | `health_reason`: text, 64 bytes |
//! | 254 | 1 | `three_a` code (0: no 3A controller runs) |
//! | 255 | 1 | `publish` code (0 waiting, 1 publishing, 2 not publishable) |
//! | 256 | 8 | `frames`: frames delivered since the service started |
//! | 264 | 8 | `last_id`: the last frame's identity |
//! | 272 | 8 | `last_timestamp_ns` |
//! | 280 | 8 | `published`: frames written to the ring |
//! | 288 | 8 | `ring_full`: frames the ring refused (no free buffer) |
//! | 296 | 4 | `readers`: readers connected to the camera's socket now |
//! | 300 | 40 | `drops`: five u64 counts, indexed by [`DropReason`] |
//! | 340 | 8 | `copies`: frame copies in this process (`styx_frame::copy_counters`) |
//! | 348 | 8 | `copy_bytes` |
//! | 356 | 8 | `age.count`: frames in the window |
//! | 364 | 8 | `age.min_ns` |
//! | 372 | 8 | `age.median_ns` (upper bound of the median's power-of-two bucket, at most the max) |
//! | 380 | 8 | `age.max_ns` |
//! | 388 | 128 | `age.hist`: 32 u32 counts, bucket `i` holds ages in `[2^i, 2^(i+1))` ns |
//! | 516 | 4 | `achieved_fps_milli`: the window's frame slots (identities) over its timestamps |
//! | 520 | 8 | `window_span_ns`: first to last frame timestamp in the window |
//! | 528 | 4 | `window_frames` |
//! | 532 | 4 | `requested_fps_milli` (0 when not given) |
//! | 536 | 65 | `requested_reason`: text, 64 bytes; set only when the rates differ |
//! | 601 | 4 | `next_delay_ms`: the wait before the next attempt (0 when none) |
//! | 605 | 4 | `failures`: failures in a row |
//! | 609 | 8 | `total_failures` |
//! | 617 | 97 | `last_error`: text, 96 bytes |
//!
//! Total: 714 bytes.

use std::fmt;

/// The record's magic.
pub const MAGIC: [u8; 4] = *b"STCN";
/// The layout version this build writes and reads.
pub const VERSION: u16 = 1;
/// Drop reasons counted; the index of each is [`DropReason`].
pub const DROP_REASONS: usize = 5;
/// Buckets in the frame age histogram.
pub const AGE_BUCKETS: usize = 32;
/// The record's size in bytes.
pub const SNAPSHOT_BYTES: usize = 714;

/// Why a frame did not reach the readers, or a rate was not kept. The index into
/// [`Snapshot::drops`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum DropReason {
    /// The camera's buffer pool had no free buffer for the frame.
    PoolExhausted = 0,
    /// The frame's buffer was still held by a lease when the stream reached it.
    HeldBuffer = 1,
    /// The ring had no free buffer for the frame (every buffer published or held by a reader).
    RingFull = 2,
    /// The backend failed (the camera's failed attempts, since the service started).
    BackendError = 3,
    /// Frames the stream did not deliver, with no reason reported (a gap in the frame identities).
    Unattributed = 4,
}

impl DropReason {
    /// Every reason, in index order.
    pub const ALL: [DropReason; DROP_REASONS] = [
        DropReason::PoolExhausted,
        DropReason::HeldBuffer,
        DropReason::RingFull,
        DropReason::BackendError,
        DropReason::Unattributed,
    ];

    /// The reason's name, as the tools print it.
    pub const fn name(self) -> &'static str {
        match self {
            DropReason::PoolExhausted => "pool_exhausted",
            DropReason::HeldBuffer => "held_buffer",
            DropReason::RingFull => "ring_full",
            DropReason::BackendError => "backend_error",
            DropReason::Unattributed => "unattributed",
        }
    }
}

/// A text field of fixed size: its bytes, cut at a character boundary when longer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Text<const N: usize> {
    len: u8,
    bytes: [u8; N],
}

impl<const N: usize> Text<N> {
    /// The empty text.
    pub const fn empty() -> Self {
        Self {
            len: 0,
            bytes: [0; N],
        }
    }

    /// `text`, cut to `N` bytes at a character boundary.
    pub fn new(text: &str) -> Self {
        let mut end = text.len().min(N);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut bytes = [0_u8; N];
        bytes[..end].copy_from_slice(&text.as_bytes()[..end]);
        Self {
            len: end as u8,
            bytes,
        }
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }

    /// Whether there is no text.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> fmt::Debug for Text<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// Which backend owns the camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    None,
    Native,
    Libcamera,
    V4l2,
}

/// Where the camera's worker is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraState {
    Absent,
    Streaming,
    Retrying,
    Backoff,
    Stopped,
    Off,
}

/// The device's health, as its backend reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Unknown,
    Healthy,
    Degraded,
    Failed,
}

/// The 3A controller's state. The service runs no 3A controller yet, so the only value is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreeA {
    None,
}

/// Whether the camera's frames reach readers in other processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishState {
    Waiting,
    Publishing,
    NotPublishable,
}

/// The frame age figures over one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Age {
    /// Frames in the window.
    pub count: u64,
    pub min_ns: u64,
    pub median_ns: u64,
    pub max_ns: u64,
    pub hist: [u32; AGE_BUCKETS],
}

/// One camera's counters at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub seq: u64,
    pub taken_ns: u64,
    pub camera: Text<64>,
    pub owner: Owner,
    pub owner_reason: Text<96>,
    pub state: CameraState,
    pub health: Health,
    pub health_reason: Text<64>,
    pub three_a: ThreeA,
    pub publish: PublishState,
    pub frames: u64,
    pub last_id: u64,
    pub last_timestamp_ns: u64,
    pub published: u64,
    pub ring_full: u64,
    pub readers: u32,
    /// Counts by [`DropReason`] index.
    pub drops: [u64; DROP_REASONS],
    pub copies: u64,
    pub copy_bytes: u64,
    pub age: Age,
    /// The stream's rate over the window: frame slots over their timestamps (0 with fewer than two).
    pub achieved_fps_milli: u32,
    pub window_span_ns: u64,
    pub window_frames: u32,
    pub requested_fps_milli: u32,
    pub requested_reason: Text<64>,
    pub next_delay_ms: u32,
    pub failures: u32,
    pub total_failures: u64,
    pub last_error: Text<96>,
}

/// Why bytes are not a snapshot this build reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// Fewer bytes than the record's size.
    Short(usize),
    /// The magic is not `STCN`.
    Magic,
    /// A version this build does not read.
    Version(u16),
    /// The record's own length does not match this build's.
    Length(u16),
    /// A code no variant has.
    Code { field: &'static str, code: u8 },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::Short(n) => write!(f, "{n} bytes is shorter than a snapshot"),
            SnapshotError::Magic => f.write_str("not a counters snapshot (bad magic)"),
            SnapshotError::Version(v) => {
                write!(f, "snapshot version {v} is not read by this build")
            }
            SnapshotError::Length(n) => write!(f, "snapshot length {n} is not {SNAPSHOT_BYTES}"),
            SnapshotError::Code { field, code } => write!(f, "{field}: unknown code {code}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// Reads a snapshot from the bytes of one record (at least [`SNAPSHOT_BYTES`] long).
///
/// # Errors
///
/// [`SnapshotError`] when the bytes are short, carry another magic, version or length, or hold a
/// code this build does not know.
pub fn read_snapshot(bytes: &[u8]) -> Result<Snapshot, SnapshotError> {
    let mut take = Take { buf: bytes, at: 0 };
    if bytes.len() < SNAPSHOT_BYTES {
        return Err(SnapshotError::Short(bytes.len()));
    }
    if take.array::<4>() != MAGIC {
        return Err(SnapshotError::Magic);
    }
    let version = take.u16();
    if version != VERSION {
        return Err(SnapshotError::Version(version));
    }
    let length = take.u16();
    if usize::from(length) != SNAPSHOT_BYTES {
        return Err(SnapshotError::Length(length));
    }
    let seq = take.u64();
    let taken_ns = take.u64();
    let camera = take.text::<64>();
    let owner = match take.u8() {
        0 => Owner::None,
        1 => Owner::Native,
        2 => Owner::Libcamera,
        3 => Owner::V4l2,
        code => {
            return Err(SnapshotError::Code {
                field: "owner",
                code,
            });
        }
    };
    let owner_reason = take.text::<96>();
    let state = match take.u8() {
        0 => CameraState::Absent,
        1 => CameraState::Streaming,
        2 => CameraState::Retrying,
        3 => CameraState::Backoff,
        4 => CameraState::Stopped,
        5 => CameraState::Off,
        code => {
            return Err(SnapshotError::Code {
                field: "state",
                code,
            });
        }
    };
    let health = match take.u8() {
        0 => Health::Unknown,
        1 => Health::Healthy,
        2 => Health::Degraded,
        3 => Health::Failed,
        code => {
            return Err(SnapshotError::Code {
                field: "health",
                code,
            });
        }
    };
    let health_reason = take.text::<64>();
    let three_a = match take.u8() {
        0 => ThreeA::None,
        code => {
            return Err(SnapshotError::Code {
                field: "three_a",
                code,
            });
        }
    };
    let publish = match take.u8() {
        0 => PublishState::Waiting,
        1 => PublishState::Publishing,
        2 => PublishState::NotPublishable,
        code => {
            return Err(SnapshotError::Code {
                field: "publish",
                code,
            });
        }
    };
    let frames = take.u64();
    let last_id = take.u64();
    let last_timestamp_ns = take.u64();
    let published = take.u64();
    let ring_full = take.u64();
    let readers = take.u32();
    let drops = std::array::from_fn(|_| take.u64());
    let copies = take.u64();
    let copy_bytes = take.u64();
    let age = Age {
        count: take.u64(),
        min_ns: take.u64(),
        median_ns: take.u64(),
        max_ns: take.u64(),
        hist: std::array::from_fn(|_| take.u32()),
    };
    let achieved_fps_milli = take.u32();
    let window_span_ns = take.u64();
    let window_frames = take.u32();
    let requested_fps_milli = take.u32();
    let requested_reason = take.text::<64>();
    let next_delay_ms = take.u32();
    let failures = take.u32();
    let total_failures = take.u64();
    let last_error = take.text::<96>();
    debug_assert_eq!(take.at, SNAPSHOT_BYTES);
    Ok(Snapshot {
        seq,
        taken_ns,
        camera,
        owner,
        owner_reason,
        state,
        health,
        health_reason,
        three_a,
        publish,
        frames,
        last_id,
        last_timestamp_ns,
        published,
        ring_full,
        readers,
        drops,
        copies,
        copy_bytes,
        age,
        achieved_fps_milli,
        window_span_ns,
        window_frames,
        requested_fps_milli,
        requested_reason,
        next_delay_ms,
        failures,
        total_failures,
        last_error,
    })
}

impl Snapshot {
    /// The record's bytes. Writes into a fixed array: no allocation.
    pub fn to_bytes(&self) -> [u8; SNAPSHOT_BYTES] {
        let mut out = [0_u8; SNAPSHOT_BYTES];
        let mut put = Put {
            buf: &mut out,
            at: 0,
        };
        put.bytes(&MAGIC);
        put.u16(VERSION);
        put.u16(SNAPSHOT_BYTES as u16);
        put.u64(self.seq);
        put.u64(self.taken_ns);
        put.text(&self.camera);
        put.u8(match self.owner {
            Owner::None => 0,
            Owner::Native => 1,
            Owner::Libcamera => 2,
            Owner::V4l2 => 3,
        });
        put.text(&self.owner_reason);
        put.u8(match self.state {
            CameraState::Absent => 0,
            CameraState::Streaming => 1,
            CameraState::Retrying => 2,
            CameraState::Backoff => 3,
            CameraState::Stopped => 4,
            CameraState::Off => 5,
        });
        put.u8(match self.health {
            Health::Unknown => 0,
            Health::Healthy => 1,
            Health::Degraded => 2,
            Health::Failed => 3,
        });
        put.text(&self.health_reason);
        put.u8(match self.three_a {
            ThreeA::None => 0,
        });
        put.u8(match self.publish {
            PublishState::Waiting => 0,
            PublishState::Publishing => 1,
            PublishState::NotPublishable => 2,
        });
        put.u64(self.frames);
        put.u64(self.last_id);
        put.u64(self.last_timestamp_ns);
        put.u64(self.published);
        put.u64(self.ring_full);
        put.u32(self.readers);
        for drop in self.drops {
            put.u64(drop);
        }
        put.u64(self.copies);
        put.u64(self.copy_bytes);
        put.u64(self.age.count);
        put.u64(self.age.min_ns);
        put.u64(self.age.median_ns);
        put.u64(self.age.max_ns);
        for bucket in self.age.hist {
            put.u32(bucket);
        }
        put.u32(self.achieved_fps_milli);
        put.u64(self.window_span_ns);
        put.u32(self.window_frames);
        put.u32(self.requested_fps_milli);
        put.text(&self.requested_reason);
        put.u32(self.next_delay_ms);
        put.u32(self.failures);
        put.u64(self.total_failures);
        put.text(&self.last_error);
        debug_assert_eq!(put.at, SNAPSHOT_BYTES);
        out
    }
}

struct Put<'a> {
    buf: &'a mut [u8],
    at: usize,
}

impl Put<'_> {
    fn bytes(&mut self, bytes: &[u8]) {
        self.buf[self.at..self.at + bytes.len()].copy_from_slice(bytes);
        self.at += bytes.len();
    }
    fn u8(&mut self, v: u8) {
        self.bytes(&[v]);
    }
    fn u16(&mut self, v: u16) {
        self.bytes(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }
    fn text<const N: usize>(&mut self, t: &Text<N>) {
        self.u8(t.len);
        self.bytes(&t.bytes);
    }
}

struct Take<'a> {
    buf: &'a [u8],
    at: usize,
}

impl Take<'_> {
    fn bytes(&mut self, n: usize) -> &[u8] {
        let out = &self.buf[self.at..self.at + n];
        self.at += n;
        out
    }
    fn array<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0_u8; N];
        out.copy_from_slice(self.bytes(N));
        out
    }
    fn u8(&mut self) -> u8 {
        self.array::<1>()[0]
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.array())
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.array())
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.array())
    }
    fn text<const N: usize>(&mut self) -> Text<N> {
        let len = self.u8();
        let bytes: [u8; N] = self.array();
        // A length past the field's size is cut to it, so decoding never reads past the field.
        Text {
            len: len.min(N as u8),
            bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        let mut hist = [0_u32; AGE_BUCKETS];
        hist[20] = 7;
        Snapshot {
            seq: 9,
            taken_ns: 123_456_789,
            camera: Text::new("lemnos:cam0-sensor"),
            owner: Owner::Native,
            owner_reason: Text::new("native chain on the board"),
            state: CameraState::Backoff,
            health: Health::Degraded,
            health_reason: Text::new("link not confirmed"),
            three_a: ThreeA::None,
            publish: PublishState::Publishing,
            frames: 1000,
            last_id: 1002,
            last_timestamp_ns: 42,
            published: 990,
            ring_full: 3,
            readers: 2,
            drops: [1, 2, 3, 4, 5],
            copies: 0,
            copy_bytes: 0,
            age: Age {
                count: 7,
                min_ns: 1 << 20,
                median_ns: 1 << 21,
                max_ns: 1 << 21,
                hist,
            },
            achieved_fps_milli: 59_998,
            window_span_ns: 116_000_000,
            window_frames: 7,
            requested_fps_milli: 120_000,
            requested_reason: Text::new("sensor mode floor"),
            next_delay_ms: 500,
            failures: 2,
            total_failures: 4,
            last_error: Text::new("no frame in 4 waits"),
        }
    }

    #[test]
    fn the_record_round_trips_and_its_size_is_the_documented_one() {
        let snap = sample();
        let bytes = snap.to_bytes();
        assert_eq!(bytes.len(), SNAPSHOT_BYTES);
        assert_eq!(&bytes[..4], b"STCN");
        assert_eq!(read_snapshot(&bytes), Ok(snap));
        // The documented offsets: frames at 256, the last error's text ends the record.
        assert_eq!(
            u64::from_le_bytes(bytes[256..264].try_into().unwrap()),
            1000
        );
        assert_eq!(usize::from(bytes[617]), "no frame in 4 waits".len());
    }

    #[test]
    fn bad_bytes_are_refused_not_read() {
        let mut bytes = sample().to_bytes();
        assert_eq!(read_snapshot(&bytes[..10]), Err(SnapshotError::Short(10)));
        bytes[0] = b'X';
        assert_eq!(read_snapshot(&bytes), Err(SnapshotError::Magic));
        bytes[0] = b'S';
        bytes[4] = 2;
        assert_eq!(read_snapshot(&bytes), Err(SnapshotError::Version(2)));
        bytes[4] = 1;
        bytes[89] = 9;
        assert_eq!(
            read_snapshot(&bytes),
            Err(SnapshotError::Code {
                field: "owner",
                code: 9
            })
        );
    }

    #[test]
    fn text_is_cut_at_a_character_boundary() {
        let t: Text<3> = Text::new("ab\u{e9}cd");
        assert_eq!(t.as_str(), "ab");
        assert!(Text::<4>::new("").is_empty());
    }
}
