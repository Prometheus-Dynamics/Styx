//! MCAP import: recordings written by the `mcap` crate in the Styx layout (ROS 2 CDR, one
//! `styx/msg/FrameMeta` and one image message per frame, a clock and a pyramid channel that are
//! skipped) come back as the same frames in `styxrec`.

mod support;

use std::collections::BTreeMap;
use std::io::Cursor;

use mcap::records::{MessageHeader, Metadata};
use styx_format::{FourCc, plane_geometry};
use styx_frame::{FrameEntry, packed_layouts};
use styx_record::{RecordError, Replay, import_mcap};

use support::{Sample, samples};

/// `s` as the Styx MCAP layout carries it: the visible rows of each plane, tightly packed, plane
/// after plane (what the recorder writes and the importer checks). Compressed frames are as they
/// are.
fn packed(s: &Sample) -> Sample {
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

/// An MCAP file in the Styx layout: the `styx.recording` metadata, the channels, and one meta and
/// one image message per frame (sequence = the frame's id), with a clock and a pyramid message
/// that the import skips.
fn styx_mcap(frames: &[Sample], header: &[(&str, &str)]) -> Vec<u8> {
    let mut w = mcap::WriteOptions::new()
        .profile("ros2")
        .library("styx-record tests")
        .compression(None)
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
        w.write_to_known_channel(&header(clock_channel), &[1, 2, 3])
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
    let mcap = styx_mcap(&frames, &header_pairs());

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

    for sample in &frames {
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

#[test]
fn compressed_chunk_is_refused_rather_than_misread() {
    // Magic, then one chunk record whose compression is "zstd".
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 8 * 3 + 4]); // times, uncompressed size, CRC
    body.extend_from_slice(&4u32.to_le_bytes());
    body.extend_from_slice(b"zstd");
    body.extend_from_slice(&0u64.to_le_bytes()); // records
    let mut file = vec![0x89, b'M', b'C', b'A', b'P', 0x30, b'\r', b'\n'];
    file.push(0x06);
    file.extend_from_slice(&(body.len() as u64).to_le_bytes());
    file.extend_from_slice(&body);
    let mut out = Vec::new();
    assert!(matches!(
        import_mcap(&mut Cursor::new(file), &mut out),
        Err(RecordError::Unsupported(_))
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
