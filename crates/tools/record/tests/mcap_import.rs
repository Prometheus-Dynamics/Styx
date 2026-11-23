//! MCAP import: recordings written by the `mcap` crate in the Styx layout (ROS 2 CDR, one
//! `styx/msg/FrameMeta` and one image message per frame, a clock and a pyramid channel that are
//! skipped) come back as the same frames in `styxrec`.

use crate::support;

use std::collections::BTreeMap;
use std::io::Cursor;

use mcap::records::{MessageHeader, Metadata};
use styx_format::{FourCc, plane_geometry};
use styx_frame::{FrameEntry, packed_layouts};
use styx_record::{ImportStats, RecordError, Replay, import_mcap};

use support::{Sample, samples};

/// `s` as the Styx MCAP layout carries it: the visible rows of each plane, tightly packed, plane
/// after plane (what the recorder writes and the importer checks). Compressed frames are as they
/// are.
pub(crate) fn packed(s: &Sample) -> Sample {
    let format = s.entry.format;
    if format.code.is_compressed() {
        return s.clone();
    }
    let (w, h) = (
        format.resolution.width.get(),
        format.resolution.height.get(),
    );
    let layouts = packed_layouts(format.code, w, h, None, None).unwrap();
    let geometry = plane_geometry(format.code, w, h).unwrap();
    let mut data = vec![0u8; layouts.span().unwrap()];
    for (i, from) in s.entry.layouts.iter().enumerate() {
        let shape = geometry.get(i).unwrap();
        let to = layouts[i];
        for row in 0..shape.rows {
            let src = from.offset + row * from.stride;
            let dst = to.offset + row * to.stride;
            data[dst..dst + shape.row_bytes].copy_from_slice(&s.data[src..src + shape.row_bytes]);
        }
    }
    Sample {
        entry: FrameEntry::new(s.entry.id, s.entry.timestamp_ns, format, layouts),
        meta: s.meta,
        data,
    }
}

fn packed_samples(seed: u64) -> Vec<Sample> {
    samples(seed).iter().map(packed).collect()
}

/// A little-endian ROS 2 CDR writer: fields aligned from the end of the encapsulation header.
struct Cdr(Vec<u8>);

impl Cdr {
    fn new() -> Self {
        Self(vec![0x00, 0x01, 0x00, 0x00])
    }
    fn align(&mut self, n: usize) {
        let pos = self.0.len() - 4;
        self.0.resize(self.0.len() + (n - pos % n) % n, 0);
    }
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.align(4);
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.align(4);
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn f32(&mut self, v: f32) -> &mut Self {
        self.u32(v.to_bits())
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.align(8);
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i64(&mut self, v: i64) -> &mut Self {
        self.align(8);
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn string(&mut self, s: &str) -> &mut Self {
        self.u32(s.len() as u32 + 1);
        self.0.extend_from_slice(s.as_bytes());
        self.0.push(0);
        self
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
        self
    }
    fn finish(&self) -> Vec<u8> {
        self.0.clone()
    }
}

fn stamp(c: &mut Cdr, ts: u64) {
    c.i32((ts / 1_000_000_000) as i32);
    c.u32((ts % 1_000_000_000) as u32);
    c.string("styx");
}

/// `styx/msg/FrameMeta` version 2 for `s`: the format, timestamp, and the native sensor values.
fn frame_meta(s: &Sample) -> Vec<u8> {
    let format = s.entry.format;
    let mut c = Cdr::new();
    c.string("/styx/image");
    c.u32(format.code.to_u32());
    c.string(&format.code.to_string());
    c.u32(format.resolution.width.get());
    c.u32(format.resolution.height.get());
    c.u8(format.color.code());
    c.u64(s.entry.timestamp_ns);
    c.u8(1); // monotonic
    c.u8(3); // native
    c.u32(s.entry.id as u32);
    c.u32(0).u32(0).u32(0);
    c.u8(0); // zero copy
    c.string("");
    c.u8(0); // no crop
    c.u32(0).u32(0).u32(0).u32(0);
    for _ in 0..5 {
        c.i64(-1);
    }
    c.bytes(&[]); // pyramid levels
    c.u64(5_000_000); // exposure 5 ms
    c.f32(1.5).f32(1.0);
    c.u64(33_333_333).u32(480).u8(1).u8(0);
    c.finish()
}

fn image(s: &Sample) -> Vec<u8> {
    let mut c = Cdr::new();
    stamp(&mut c, s.entry.timestamp_ns);
    if s.entry.format.code.is_compressed() {
        c.string("jpeg");
        c.bytes(&s.data);
    } else {
        let format = s.entry.format;
        c.u32(format.resolution.height.get());
        c.u32(format.resolution.width.get());
        c.string("raw");
        c.u8(0);
        c.u32(0);
        c.bytes(&s.data[..s.entry.layouts.span().unwrap()]);
    }
    c.finish()
}

/// A `styx/msg/ClockSample` message (CDR): monotonic, boottime, realtime, read spread.
pub(crate) fn clock_sample(monotonic_ns: u64, boottime_ns: u64) -> Vec<u8> {
    let mut out = vec![0x00, 0x01, 0x00, 0x00];
    out.extend_from_slice(&monotonic_ns.to_le_bytes());
    out.extend_from_slice(&boottime_ns.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

/// An MCAP file in the Styx layout: the `styx.recording` metadata, the channels, and one meta and
/// one image message per frame (sequence = the frame's id), with a clock and a pyramid message
/// that the import skips.
fn styx_mcap(frames: &[Sample], header: &[(&str, &str)]) -> Vec<u8> {
    styx_mcap_with(frames, header, None, None)
}

/// [`styx_mcap`], with the writer's chunk compression and chunk size.
fn styx_mcap_with(
    frames: &[Sample],
    header: &[(&str, &str)],
    compression: Option<mcap::Compression>,
    chunk_size: Option<u64>,
) -> Vec<u8> {
    let mut w = mcap::WriteOptions::new()
        .profile("ros2")
        .library("styx-record tests")
        .compression(compression)
        .chunk_size(chunk_size)
        .create(Cursor::new(Vec::new()))
        .unwrap();
    let metadata: BTreeMap<String, String> = header
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    w.write_metadata(&Metadata {
        name: "styx.recording".into(),
        metadata,
    })
    .unwrap();
    let schema = w
        .add_schema("styx/msg/FrameMeta", "ros2msg", b"# FrameMeta")
        .unwrap();
    let meta_channel = w
        .add_channel(schema, "/styx/frame_meta", "cdr", &BTreeMap::new())
        .unwrap();
    let image_schema = w
        .add_schema("sensor_msgs/msg/Image", "ros2msg", b"# Image")
        .unwrap();
    let raw_channel = w
        .add_channel(image_schema, "/styx/image", "cdr", &BTreeMap::new())
        .unwrap();
    let compressed_channel = w
        .add_channel(
            image_schema,
            "/styx/image/compressed",
            "cdr",
            &BTreeMap::new(),
        )
        .unwrap();
    let clock_channel = w
        .add_channel(schema, "/styx/clock", "cdr", &BTreeMap::new())
        .unwrap();
    let pyramid_channel = w
        .add_channel(image_schema, "/styx/pyramid/0", "cdr", &BTreeMap::new())
        .unwrap();

    for (i, s) in frames.iter().enumerate() {
        let seq = i as u32;
        let log_time = s.entry.timestamp_ns;
        let header = |channel_id| MessageHeader {
            channel_id,
            sequence: seq,
            log_time,
            publish_time: log_time,
        };
        w.write_to_known_channel(&header(meta_channel), &frame_meta(s))
            .unwrap();
        let image_channel = if s.entry.format.code.is_compressed() {
            compressed_channel
        } else {
            raw_channel
        };
        w.write_to_known_channel(&header(image_channel), &image(s))
            .unwrap();
        // A pyramid level and a clock sample: not frames, skipped.
        w.write_to_known_channel(&header(pyramid_channel), &Cdr::new().finish())
            .unwrap();
        w.write_to_known_channel(&header(clock_channel), &clock_sample(log_time, log_time))
            .unwrap();
    }
    // A frame whose image never arrives: never emitted.
    let orphan = MessageHeader {
        channel_id: meta_channel,
        sequence: 999,
        log_time: 1,
        publish_time: 1,
    };
    w.write_to_known_channel(&orphan, &frame_meta(&frames[0]))
        .unwrap();
    w.finish().unwrap();
    w.into_inner().into_inner()
}

fn header_pairs() -> Vec<(&'static str, &'static str)> {
    vec![
        ("styx_format_version", "2"),
        ("device", "bench camera"),
        ("device_keys", "sensor-1\nserial-42"),
        ("backend", "native"),
        ("fourcc", "1497715271"), // GREY
        ("width", "64"),
        ("height", "32"),
        ("color", "0"),
        ("interval", "1/30"),
    ]
}

#[test]
fn import_round_trips_frames_metadata_and_bytes() {
    let frames = packed_samples(3);
    round_trip(&frames, styx_mcap(&frames, &header_pairs()));
}

/// The MCAP writer's lz4 chunks (its default compression, and HeliOS's recorder's), one chunk
/// for the file and a chunk every few messages, read as the uncompressed file is.
#[cfg(feature = "mcap-lz4")]
#[test]
fn import_reads_lz4_chunks_as_the_writer_makes_them() {
    let frames = packed_samples(6);
    for chunk_size in [None, Some(4096)] {
        let mcap = styx_mcap_with(
            &frames,
            &header_pairs(),
            Some(mcap::Compression::Lz4),
            chunk_size,
        );
        round_trip(&frames, mcap);
    }
}

fn round_trip(frames: &[Sample], mcap: Vec<u8>) {
    let mut out = Vec::new();
    let stats = import_mcap(&mut Cursor::new(mcap), &mut out).unwrap();
    assert_eq!(stats.frames, frames.len() as u64);
    assert!(!stats.truncated);
    let largest = frames
        .iter()
        .map(|s| s.entry.layouts.span().unwrap() as u64)
        .max()
        .unwrap();
    assert_eq!(stats.payload, largest);

    let mut replay = Replay::new(Cursor::new(out)).unwrap();
    let header = replay.header().clone();
    assert_eq!(header.device, "bench camera");
    assert_eq!(header.keys, vec!["sensor-1", "serial-42"]);
    assert_eq!(header.backend, "native");
    assert_eq!(header.format.code, FourCc::GREY);
    assert_eq!(header.format.resolution.width.get(), 64);
    assert_eq!(header.interval.unwrap().numerator.get(), 1);
    assert_eq!(header.interval.unwrap().denominator.get(), 30);

    for sample in frames {
        let lease = replay.next_frame().unwrap().expect("an imported frame");
        assert_eq!(*lease.entry(), sample.entry);
        assert_eq!(lease.meta().sensor_timestamp_ns, sample.entry.timestamp_ns);
        assert_eq!(lease.meta().exposure_us, 5_000);
        assert_eq!(lease.meta().analogue_gain_milli, 1_500);
        assert_eq!(lease.meta().digital_gain_milli, 1_000);
        let span = sample.entry.layouts.span().unwrap();
        let read = lease.read().unwrap();
        assert_eq!(
            &read.bytes()[..span],
            &sample.data[..span],
            "frame bytes differ"
        );
    }
    assert!(replay.next_frame().unwrap().is_none());
    assert_eq!(replay.delivered(), frames.len() as u64);
}

#[test]
fn import_keeps_colour_and_compressed_frames() {
    let frames = packed_samples(4);
    let mcap = styx_mcap(&frames, &header_pairs());
    let mut out = Vec::new();
    import_mcap(&mut Cursor::new(mcap), &mut out).unwrap();
    let mut replay = Replay::new(Cursor::new(out)).unwrap();
    let mut seen = 0;
    while let Some(lease) = replay.next_frame().unwrap() {
        let original = &frames[seen];
        assert_eq!(lease.format().color, original.entry.format.color);
        assert_eq!(lease.format().code, original.entry.format.code);
        seen += 1;
    }
    assert_eq!(seen, frames.len());
    assert_eq!(
        frames[3].entry.format.code,
        FourCc::new(*b"MJPG"),
        "the last sample is the bitstream"
    );
}

#[test]
fn file_without_styx_metadata_is_refused() {
    let frames = samples(5);
    let mut w = mcap::WriteOptions::new()
        .compression(None)
        .create(Cursor::new(Vec::new()))
        .unwrap();
    let schema = w.add_schema("x", "ros2msg", b"").unwrap();
    let channel = w
        .add_channel(schema, "/styx/image", "cdr", &BTreeMap::new())
        .unwrap();
    w.write_to_known_channel(
        &MessageHeader {
            channel_id: channel,
            sequence: 0,
            log_time: 0,
            publish_time: 0,
        },
        &image(&frames[0]),
    )
    .unwrap();
    w.finish().unwrap();
    let bytes = w.into_inner().into_inner();
    let mut out = Vec::new();
    assert!(matches!(
        import_mcap(&mut Cursor::new(bytes), &mut out),
        Err(RecordError::Corrupt(_))
    ));
}

#[test]
fn not_an_mcap_file_is_refused() {
    let mut out = Vec::new();
    assert!(matches!(
        import_mcap(
            &mut Cursor::new(b"STYXRCN1 but not MCAP at all".to_vec()),
            &mut out
        ),
        Err(RecordError::NotARecording)
    ));
}

/// A file of one chunk record: magic, then the chunk with `compression`, its stated
/// uncompressed `size` and `crc`, and `records` as its (compressed) records.
fn one_chunk(compression: &str, size: u64, crc: u32, records: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 8 * 2]); // times
    body.extend_from_slice(&size.to_le_bytes());
    body.extend_from_slice(&crc.to_le_bytes());
    body.extend_from_slice(&(compression.len() as u32).to_le_bytes());
    body.extend_from_slice(compression.as_bytes());
    body.extend_from_slice(&(records.len() as u64).to_le_bytes());
    body.extend_from_slice(records);
    let mut file = vec![0x89, b'M', b'C', b'A', b'P', 0x30, b'\r', b'\n'];
    file.push(0x06);
    file.extend_from_slice(&(body.len() as u64).to_le_bytes());
    file.extend_from_slice(&body);
    file
}

fn import_of(file: Vec<u8>) -> Result<ImportStats, RecordError> {
    let mut out = Vec::new();
    import_mcap(&mut Cursor::new(file), &mut out)
}

/// Without the decoder, an lz4 chunk is refused by name, never misread.
#[cfg(not(feature = "mcap-lz4"))]
#[test]
fn an_lz4_chunk_is_refused_without_the_feature() {
    let frames = packed_samples(2);
    let mcap = styx_mcap_with(&frames, &header_pairs(), Some(mcap::Compression::Lz4), None);
    assert!(matches!(
        import_of(mcap),
        Err(RecordError::Unsupported(why)) if why.contains("mcap-lz4")
    ));
}

#[test]
fn compressed_chunk_is_refused_rather_than_misread() {
    assert!(matches!(
        import_of(one_chunk("zstd", 0, 0, &[])),
        Err(RecordError::Unsupported(_))
    ));
    assert!(matches!(
        import_of(one_chunk("brotli", 0, 0, &[])),
        Err(RecordError::Unsupported(_))
    ));
}

/// An lz4 chunk that does not decompress, is not the size it states, fails its CRC, or states a
/// size past the limit is refused, never read.
#[cfg(feature = "mcap-lz4")]
#[test]
fn a_bad_lz4_chunk_is_refused() {
    // One channel record (opcode 4): id 1, schema 0, topic "/x", encoding "", metadata {}.
    let mut channel = vec![4u8];
    let mut fields = Vec::new();
    fields.extend_from_slice(&1u16.to_le_bytes());
    fields.extend_from_slice(&0u16.to_le_bytes());
    fields.extend_from_slice(&2u32.to_le_bytes());
    fields.extend_from_slice(b"/x");
    fields.extend_from_slice(&0u32.to_le_bytes());
    fields.extend_from_slice(&0u32.to_le_bytes());
    channel.extend_from_slice(&(fields.len() as u64).to_le_bytes());
    channel.extend_from_slice(&fields);
    let mut lz4 = lz4_flex::frame::FrameEncoder::new(Vec::new());
    std::io::Write::write_all(&mut lz4, &channel).unwrap();
    let packed = lz4.finish().unwrap();
    let size = channel.len() as u64;
    let crc = {
        // CRC-32 (IEEE), as MCAP states it.
        let mut c = !0u32;
        for &b in &channel {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    };

    // The good chunk reads (to the end: the file has no styx.recording metadata).
    for good_crc in [crc, 0] {
        assert!(matches!(
            import_of(one_chunk("lz4", size, good_crc, &packed)),
            Err(RecordError::Corrupt(
                "MCAP file has no styx.recording metadata"
            ))
        ));
    }
    assert!(matches!(
        import_of(one_chunk("lz4", size, crc ^ 1, &packed)),
        Err(RecordError::Corrupt("MCAP lz4 chunk fails its CRC"))
    ));
    for wrong in [size - 1, size + 1] {
        assert!(matches!(
            import_of(one_chunk("lz4", wrong, 0, &packed)),
            Err(RecordError::Corrupt(
                "MCAP lz4 chunk is not the size it states"
            ))
        ));
    }
    let mut broken = packed.clone();
    let last = broken.len() - 6;
    broken[last] ^= 0xFF;
    broken.truncate(broken.len() - 3);
    assert!(matches!(
        import_of(one_chunk("lz4", size, 0, &broken)),
        Err(RecordError::Corrupt(_))
    ));
    assert!(matches!(
        import_of(one_chunk("lz4", 1 << 40, 0, &packed)),
        Err(RecordError::TooLarge("MCAP chunk"))
    ));
}

#[test]
fn recording_cut_short_imports_what_it_has() {
    let frames = packed_samples(6);
    let mcap = styx_mcap(&frames, &header_pairs());
    let mut seen_truncated = false;
    for cut in (mcap.len() / 2..mcap.len()).step_by(37) {
        let mut out = Vec::new();
        match import_mcap(&mut Cursor::new(mcap[..cut].to_vec()), &mut out) {
            Ok(stats) => {
                assert!(stats.frames <= frames.len() as u64);
                seen_truncated |= stats.truncated;
                let mut replay = Replay::new(Cursor::new(out)).unwrap();
                let mut n = 0;
                while replay.next_frame().unwrap().is_some() {
                    n += 1;
                }
                assert_eq!(n as u64, stats.frames);
            }
            Err(e) => panic!("cut {cut} of {} should import what it has: {e}", mcap.len()),
        }
    }
    assert!(seen_truncated);
}

/// A file as HeliOS's recorder writes it: frames in the Styx layout, `/styx/clock` records,
/// a Lemnos topic on `CLOCK_BOOTTIME` (JSON, no schema), a topic with a schema on
/// `CLOCK_MONOTONIC`, an attachment, a second metadata record and a pyramid level. The data
/// topics come back as channels with their times moved to `CLOCK_MONOTONIC`, and what is not
/// carried is counted.
#[test]
fn helios_data_topics_import_as_channels() {
    let frames = packed_samples(41);
    const SUSPENDED: u64 = 5_000_000_000;
    let mut w = mcap::WriteOptions::new()
        .profile("ros2")
        .compression(None)
        .create(Cursor::new(Vec::new()))
        .unwrap();
    let metadata: BTreeMap<String, String> = header_pairs()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    w.write_metadata(&Metadata {
        name: "styx.recording".into(),
        metadata,
    })
    .unwrap();
    w.write_metadata(&Metadata {
        name: "helios.calibration".into(),
        metadata: BTreeMap::from([("fx".into(), "600".into())]),
    })
    .unwrap();
    let schema = w.add_schema("styx/msg/FrameMeta", "ros2msg", b"#").unwrap();
    let meta_channel = w
        .add_channel(schema, "/styx/frame_meta", "cdr", &BTreeMap::new())
        .unwrap();
    let raw_channel = w
        .add_channel(schema, "/styx/image", "cdr", &BTreeMap::new())
        .unwrap();
    let compressed_channel = w
        .add_channel(schema, "/styx/image/compressed", "cdr", &BTreeMap::new())
        .unwrap();
    let clock_channel = w
        .add_channel(schema, "/styx/clock", "cdr", &BTreeMap::new())
        .unwrap();
    let pyramid_channel = w
        .add_channel(schema, "/styx/pyramid/1", "cdr", &BTreeMap::new())
        .unwrap();
    let imu_metadata = BTreeMap::from([
        ("source".to_string(), "lemnosd".to_string()),
        ("clock".to_string(), "CLOCK_BOOTTIME".to_string()),
    ]);
    let imu = w
        .add_channel(0, "/helios/imu", "json", &imu_metadata)
        .unwrap();
    let pose_schema = w
        .add_schema("helios.Pose", "jsonschema", br#"{"type":"object"}"#)
        .unwrap();
    let pose = w
        .add_channel(pose_schema, "/helios/pose", "json", &BTreeMap::new())
        .unwrap();
    w.attach(&mcap::Attachment {
        log_time: 0,
        create_time: 0,
        name: "notes.txt".into(),
        media_type: "text/plain".into(),
        data: std::borrow::Cow::Borrowed(b"hello"),
    })
    .unwrap();

    let header = |channel_id, sequence, log_time, publish_time| MessageHeader {
        channel_id,
        sequence,
        log_time,
        publish_time,
    };
    let t0 = frames[0].entry.timestamp_ns;
    // The clocks before any data: boottime runs SUSPENDED ahead.
    w.write_to_known_channel(
        &header(clock_channel, 0, t0 - 1, t0 - 1),
        &clock_sample(t0 - 1, t0 - 1 + SUSPENDED),
    )
    .unwrap();
    let mut expected_imu = Vec::new();
    for (i, s) in frames.iter().enumerate() {
        let t = s.entry.timestamp_ns;
        let seq = i as u32;
        w.write_to_known_channel(&header(meta_channel, seq, t, t), &frame_meta(s))
            .unwrap();
        let image_channel = if s.entry.format.code.is_compressed() {
            compressed_channel
        } else {
            raw_channel
        };
        w.write_to_known_channel(&header(image_channel, seq, t, t), &image(s))
            .unwrap();
        w.write_to_known_channel(&header(pyramid_channel, seq, t, t), &image(s))
            .unwrap();
        let body = format!(r#"{{"device":"imu0","t_boottime_us":{},"values":{{}}}}"#, i);
        let boot = t + SUSPENDED + 1_000;
        // The publish time differs from the log time on the first one.
        let publish = if i == 0 { boot + 7 } else { boot };
        w.write_to_known_channel(&header(imu, seq, boot, publish), body.as_bytes())
            .unwrap();
        expected_imu.push((t + 1_000, body.into_bytes()));
        // The pose's sequences are not its message count: lost on import.
        w.write_to_known_channel(&header(pose, seq + 100, t + 2_000, t + 2_000), b"{}")
            .unwrap();
    }
    w.finish().unwrap();
    let file = w.into_inner().into_inner();

    let mut out = Vec::new();
    let stats = import_mcap(&mut Cursor::new(file), &mut out).unwrap();
    assert_eq!(stats.frames, frames.len() as u64);
    assert_eq!(
        (stats.channels, stats.samples),
        (2, 2 * frames.len() as u64)
    );
    assert_eq!(stats.clock_records, 1);
    let l = stats.losses;
    assert_eq!(l.attachments, 1);
    assert_eq!(l.metadata_records, 1);
    assert_eq!(l.pyramid_messages, frames.len() as u64);
    assert_eq!(l.data_sequences, frames.len() as u64);
    assert_eq!(l.data_publish_times, 1);
    assert_eq!(l.boottime_assumed_equal, 0);

    let mut replay = Replay::new(Cursor::new(out)).unwrap();
    let imu_channel = replay.channel_named("/helios/imu").unwrap().clone();
    assert_eq!(imu_channel.spec.encoding, "json");
    assert_eq!(imu_channel.spec.clock, styx_record::SourceClock::Boottime);
    assert_eq!(imu_channel.spec.metadata, imu_metadata);
    assert!(imu_channel.spec.schema.is_empty());
    assert_eq!(
        imu_channel.declared.boottime_ns - imu_channel.declared.monotonic_ns,
        SUSPENDED
    );
    let pose_channel = replay.channel_named("/helios/pose").unwrap().clone();
    assert_eq!(pose_channel.spec.schema_name, "helios.Pose");
    assert_eq!(pose_channel.spec.schema_encoding, "jsonschema");
    assert_eq!(pose_channel.spec.clock, styx_record::SourceClock::Monotonic);

    let mut got_imu = Vec::new();
    let mut frames_seen = 0;
    while let Some(item) = replay.next_item().unwrap() {
        match item {
            styx_record::Item::Frame(_) => frames_seen += 1,
            styx_record::Item::Sample(s) if s.channel == imu_channel.id => {
                assert!(!s.late);
                got_imu.push((s.timestamp_ns, s.data.to_vec()));
            }
            styx_record::Item::Sample(_) => {}
        }
    }
    assert_eq!(frames_seen, frames.len());
    assert_eq!(got_imu, expected_imu, "IMU times moved by the clock record");
}

/// A `CLOCK_BOOTTIME` topic in a file with no `/styx/clock` record: the clocks are taken as
/// equal, and the import says how many samples that touched.
#[test]
fn boottime_without_clock_records_is_assumed_equal_and_counted() {
    let frames = packed_samples(43);
    let mut w = mcap::WriteOptions::new()
        .compression(None)
        .create(Cursor::new(Vec::new()))
        .unwrap();
    let metadata: BTreeMap<String, String> = header_pairs()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    w.write_metadata(&Metadata {
        name: "styx.recording".into(),
        metadata,
    })
    .unwrap();
    let imu = w
        .add_channel(
            0,
            "/helios/imu",
            "json",
            &BTreeMap::from([("clock".to_string(), "CLOCK_BOOTTIME".to_string())]),
        )
        .unwrap();
    for i in 0..3u64 {
        let h = MessageHeader {
            channel_id: imu,
            sequence: i as u32,
            log_time: 100 + i,
            publish_time: 100 + i,
        };
        w.write_to_known_channel(&h, b"{}").unwrap();
    }
    w.finish().unwrap();
    let mut out = Vec::new();
    let stats = import_mcap(&mut Cursor::new(w.into_inner().into_inner()), &mut out).unwrap();
    assert_eq!(stats.samples, 3);
    assert_eq!(stats.losses.boottime_assumed_equal, 3);
    let _ = frames;
}
