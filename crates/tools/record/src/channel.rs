//! Data channels: streams of timestamped samples recorded beside the frames (an IMU, a
//! gyro, a robot's pose, a message from another process).
//!
//! A channel is declared once ([`crate::Recorder::declare`]), with a name, how its samples are
//! encoded and the schema that describes them, and the clock its producer stamps on. Each
//! sample is then a timestamp and opaque bytes. Styx reads no sensors itself: the consumer that
//! has the data writes it. The fields follow MCAP's channel and schema records, so a channel
//! exports to MCAP as one.

use crate::clock::ClockPair;

/// The clock a channel's producer stamps its samples on. Samples are recorded on
/// `CLOCK_MONOTONIC` whatever this says (a `Boottime` producer's times are converted with
/// [`crate::monotonic_from_boottime`] before they are recorded); it is kept so the recording says
/// where the times came from, and with the declaration's [`ClockPair`] how they were moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SourceClock {
    /// `CLOCK_MONOTONIC`: recorded as stamped.
    #[default]
    Monotonic,
    /// `CLOCK_BOOTTIME`: converted to `CLOCK_MONOTONIC` before recording.
    Boottime,
}

impl SourceClock {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Monotonic => 0,
            Self::Boottime => 1,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Monotonic),
            1 => Some(Self::Boottime),
            _ => None,
        }
    }
}

/// What a channel is: its name, the encoding of its samples, and their schema. The names
/// follow MCAP: `encoding` is a message encoding (`json`, `cdr`, `protobuf`, `flatbuffer`, or
/// empty for bytes the reader knows by the channel's name), and the schema is named, has its
/// own encoding (`jsonschema`, `ros2msg`, `protobuf`, ..., or empty for none) and its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChannelSpec {
    /// The channel's name, unique in a recording (for example `/imu`).
    pub name: String,
    /// How each sample's bytes are encoded.
    pub encoding: String,
    /// The schema's name (for example a message type).
    pub schema_name: String,
    /// How the schema is written.
    pub schema_encoding: String,
    /// The schema's bytes; empty for none.
    pub schema: Vec<u8>,
    /// The clock the producer stamps on.
    pub clock: SourceClock,
}

/// A channel's number in its recording: declarations are numbered from zero in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelId(pub(crate) u16);

impl ChannelId {
    /// The number.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// A declared channel: its number, what it is, and both clocks read when it was declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    /// The channel's number.
    pub id: ChannelId,
    /// What it is.
    pub spec: ChannelSpec,
    /// `CLOCK_MONOTONIC` and `CLOCK_BOOTTIME` when it was declared: the offset a `Boottime`
    /// producer's samples were moved by, kept for audit.
    pub declared: ClockPair,
}

/// One sample as replayed: its channel, its `CLOCK_MONOTONIC` time and its bytes (borrowed from
/// the replay until the next call).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample<'a> {
    /// The channel it belongs to.
    pub channel: ChannelId,
    /// When it was taken, `CLOCK_MONOTONIC` nanoseconds.
    pub timestamp_ns: u64,
    /// Its bytes, as recorded.
    pub data: &'a [u8],
    /// It is older than a frame already handed out: it reached the recording after that frame
    /// had been read past. It still comes out, in the recording's order, and is counted in
    /// [`crate::Replay::late_samples`].
    pub late: bool,
}
