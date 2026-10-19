//! What an MCAP recording holds besides frames: the host's channels, messages, metadata and
//! attachments, and `/styx/clock` (`styx/msg/ClockSample`): the monotonic, boot-time and
//! real-time clocks read together, to put timestamps on one clock onto another offline.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufWriter;
use std::time::{Duration, Instant};

use mcap::records::{MessageHeader, Metadata};
use styx_core::prelude::TimestampClock;

use super::cdr::CdrWriter;
use super::mcap_format::{McapRecorder, mcap_error};
use super::{ChannelSchema, RecordCompression, ReplayError};

pub(super) const CLOCK_TOPIC: &str = "/styx/clock";

const CLOCK_SAMPLE_DEF: &str =
    "# Clocks read together (nanoseconds); monotonic is the middle of the reads.
uint64 monotonic_ns
uint64 boottime_ns
uint64 realtime_ns
uint32 read_spread_ns   # how long the reads took";

/// The MCAP library's writer options for `options` (compression, chunks).
pub(super) fn write_options(
    options: &super::RecorderOptions,
) -> Result<mcap::WriteOptions, ReplayError> {
    let w = mcap::WriteOptions::new()
        .compression(compression(options.compression)?)
        .chunk_size(options.chunk_size);
    #[cfg(any(feature = "replay-mcap-lz4", feature = "replay-mcap-zstd"))]
    let w = w.compression_level(match options.compression {
        RecordCompression::Zstd { level } => level.max(1),
        _ => 0,
    });
    #[cfg(feature = "replay-mcap-zstd")]
    let w = w.compression_threads(options.compression_threads);
    Ok(w)
}

/// The MCAP library's chunk compression for `c`; an error when its feature is off.
fn compression(c: RecordCompression) -> Result<Option<mcap::Compression>, ReplayError> {
    #[cfg_attr(
        all(feature = "replay-mcap-lz4", feature = "replay-mcap-zstd"),
        allow(unused_variables)
    )]
    let off = |feature: &str| -> Result<Option<mcap::Compression>, ReplayError> {
        Err(ReplayError::Mcap(format!(
            "{c:?} compression needs feature `{feature}`"
        )))
    };
    match c {
        RecordCompression::None => Ok(None),
        #[cfg(feature = "replay-mcap-lz4")]
        RecordCompression::Lz4 => Ok(Some(mcap::Compression::Lz4)),
        #[cfg(not(feature = "replay-mcap-lz4"))]
        RecordCompression::Lz4 => off("replay-mcap-lz4"),
        #[cfg(feature = "replay-mcap-zstd")]
        RecordCompression::Zstd { .. } => Ok(Some(mcap::Compression::Zstd)),
        #[cfg(not(feature = "replay-mcap-zstd"))]
        RecordCompression::Zstd { .. } => off("replay-mcap-zstd"),
    }
}

/// `/styx/clock`: its channel and when the next sample is due.
pub(crate) struct ClockRecord {
    channel: u16,
    period: Duration,
    next: Instant,
    sequence: u32,
}

impl ClockRecord {
    pub(super) fn add(
        writer: &mut mcap::Writer<BufWriter<File>>,
        period: Duration,
    ) -> Result<Self, ReplayError> {
        let schema = writer
            .add_schema(
                "styx/msg/ClockSample",
                "ros2msg",
                CLOCK_SAMPLE_DEF.as_bytes(),
            )
            .map_err(mcap_error)?;
        let channel = writer
            .add_channel(schema, CLOCK_TOPIC, "cdr", &BTreeMap::new())
            .map_err(mcap_error)?;
        Ok(Self {
            channel,
            period: period.max(Duration::from_millis(10)),
            next: Instant::now(),
            sequence: 0,
        })
    }
}

/// The three clocks now, as a `styx/msg/ClockSample` message, and its monotonic time.
fn clock_sample() -> (u64, Vec<u8>) {
    let read = |c: TimestampClock| c.now_ns().unwrap_or(0);
    let before = read(TimestampClock::Monotonic);
    let boot = read(TimestampClock::Boottime);
    let real = read(TimestampClock::Realtime);
    let after = read(TimestampClock::Monotonic);
    let mono = before + (after - before) / 2;
    let mut w = CdrWriter::with_capacity(32);
    w.u64(mono);
    w.u64(boot);
    w.u64(real);
    w.u32((after - before).min(u64::from(u32::MAX)) as u32);
    (mono, w.finish())
}

impl McapRecorder {
    /// A clock sample when one is due.
    pub(super) fn clock_if_due(&mut self) -> Result<(), ReplayError> {
        match &self.clock {
            Some(c) if Instant::now() >= c.next => self.clock_now(),
            _ => Ok(()),
        }
    }

    /// A clock sample now (when they are on).
    pub(super) fn clock_now(&mut self) -> Result<(), ReplayError> {
        let Some(c) = self.clock.as_mut() else {
            return Ok(());
        };
        let (mono, data) = clock_sample();
        let header = MessageHeader {
            channel_id: c.channel,
            sequence: c.sequence,
            log_time: mono,
            publish_time: mono,
        };
        c.sequence = c.sequence.wrapping_add(1);
        c.next = Instant::now() + c.period;
        self.writer
            .write_to_known_channel(&header, &data)
            .map_err(mcap_error)
    }

    /// How long until the next clock sample (`None`: samples are off).
    pub(super) fn clock_wait(&self) -> Option<Duration> {
        self.clock
            .as_ref()
            .map(|c| c.next.saturating_duration_since(Instant::now()))
    }

    pub(super) fn add_host_channel(
        &mut self,
        id: u16,
        topic: &str,
        schema: Option<&ChannelSchema>,
        message_encoding: &str,
        metadata: &BTreeMap<String, String>,
    ) -> Result<(), ReplayError> {
        let schema_id = match schema {
            Some(s) => self
                .writer
                .add_schema(&s.name, &s.encoding, &s.data)
                .map_err(mcap_error)?,
            None => 0,
        };
        self.writer
            .add_channel_with_id(id, schema_id, topic, message_encoding, metadata)
            .map_err(mcap_error)?;
        Ok(())
    }

    pub(super) fn write_host(
        &mut self,
        channel_id: u16,
        sequence: u32,
        log_time: u64,
        publish_time: u64,
        data: &[u8],
    ) -> Result<(), ReplayError> {
        let header = MessageHeader {
            channel_id,
            sequence,
            log_time,
            publish_time,
        };
        self.writer
            .write_to_known_channel(&header, data)
            .map_err(mcap_error)
    }

    pub(super) fn metadata(
        &mut self,
        name: String,
        metadata: BTreeMap<String, String>,
    ) -> Result<(), ReplayError> {
        self.writer
            .write_metadata(&Metadata { name, metadata })
            .map_err(mcap_error)
    }

    pub(super) fn attach(
        &mut self,
        name: String,
        media_type: String,
        log_time: u64,
        data: Vec<u8>,
    ) -> Result<(), ReplayError> {
        let create_time = TimestampClock::Realtime.now_ns().unwrap_or(0);
        self.writer
            .attach(&mcap::Attachment {
                log_time,
                create_time,
                name,
                media_type,
                data: Cow::Owned(data),
            })
            .map_err(mcap_error)
    }
}
