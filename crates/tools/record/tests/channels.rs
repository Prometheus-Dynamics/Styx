//! Data channels: declarations and samples recorded beside frames, replayed in timestamp order
//! within one frame of lookahead; late samples come out in file order, marked and counted;
//! version 1 files still read; damaged or out-of-order channel records are refused.

use crate::support;

use std::io::Cursor;

use styx_record::{
    ChannelSpec, ClockPair, Item, RecordError, Recorder, RecordingHeader, Replay, SourceClock,
    format,
};

use support::{Sample, fps30, payload_of, samples};

fn header_for(frames: &[Sample]) -> RecordingHeader {
    RecordingHeader {
        device: "sim camera".into(),
        keys: vec!["sensor-1".into()],
        backend: "native".into(),
        format: frames[0].entry.format,
        interval: fps30(),
        payload: payload_of(frames),
    }
}

fn imu() -> ChannelSpec {
    ChannelSpec {
        name: "/imu".into(),
        encoding: "json".into(),
        schema_name: "Imu".into(),
        schema_encoding: "jsonschema".into(),
        schema: br#"{"type":"object"}"#.to_vec(),
        clock: SourceClock::Boottime,
        metadata: [("source".to_string(), "lemnosd".to_string())].into(),
    }
}

fn pose() -> ChannelSpec {
    ChannelSpec {
        name: "/pose".into(),
        encoding: String::new(),
        clock: SourceClock::Monotonic,
        ..ChannelSpec::default()
    }
}

/// What next_item gave, owned: a frame's timestamp, or a sample's channel, time, bytes and
/// lateness.
#[derive(Debug, PartialEq, Eq)]
enum Got {
    Frame(u64),
    Sample(u16, u64, Vec<u8>, bool),
}

fn items(bytes: Vec<u8>) -> (Vec<Got>, Replay<Cursor<Vec<u8>>>) {
    let mut replay = Replay::new(Cursor::new(bytes)).unwrap();
    let mut out = Vec::new();
    while let Some(item) = replay.next_item().unwrap() {
        out.push(match item {
            Item::Frame(f) => Got::Frame(f.timestamp_ns()),
            Item::Sample(s) => {
                Got::Sample(s.channel.get(), s.timestamp_ns, s.data.to_vec(), s.late)
            }
        });
    }
    (out, replay)
}

/// Frame times are 1.0 s + i * 33.3 ms (support::samples). Samples are written as a consumer
/// sees them: an IMU sample arrives about when it is taken, a frame some time after its
/// timestamp, so samples newer than a frame often reach the file before it.
#[test]
fn frames_and_samples_come_back_in_timestamp_order() {
    let frames = samples(3);
    let t: Vec<u64> = frames.iter().map(|f| f.entry.timestamp_ns).collect();
    let mut recorder = Recorder::new(Vec::new(), &header_for(&frames)).unwrap();
    let imu = recorder.declare(imu()).unwrap();
    let pose = recorder.declare(pose()).unwrap();
    assert_eq!((imu.get(), pose.get()), (0, 1));

    recorder.sample(imu, t[0] - 5, b"a").unwrap();
    recorder.sample(imu, t[0] + 5, b"b").unwrap(); // newer than frame 0, written before it
    recorder
        .record_frame(&frames[0].entry, &frames[0].meta, &frames[0].data)
        .unwrap();
    recorder.sample(pose, t[0], b"p0").unwrap(); // at frame 0's exact time, written after it
    recorder.sample(imu, t[1] - 1, b"c").unwrap();
    recorder
        .record_frame(&frames[1].entry, &frames[1].meta, &frames[1].data)
        .unwrap();
    recorder
        .record_frame(&frames[2].entry, &frames[2].meta, &frames[2].data)
        .unwrap();
    recorder.sample(imu, t[1] + 1, b"d").unwrap(); // between 1 and 2, written after 2
    recorder.sample(imu, t[2] + 1, b"e").unwrap();
    recorder
        .record_frame(&frames[3].entry, &frames[3].meta, &frames[3].data)
        .unwrap();
    assert_eq!((recorder.frames(), recorder.samples()), (4, 6));
    let bytes = recorder.finish().unwrap();

    let (got, replay) = items(bytes);
    let s = |c: u16, ts: u64, b: &[u8], late: bool| Got::Sample(c, ts, b.to_vec(), late);
    assert_eq!(
        got,
        vec![
            s(0, t[0] - 5, b"a", false),
            // At frame 0's exact time, written after it: it still comes first.
            s(1, t[0], b"p0", false),
            Got::Frame(t[0]),
            // Newer than frame 0 but written before it: held until frame 0 is out.
            s(0, t[0] + 5, b"b", false),
            s(0, t[1] - 1, b"c", false),
            Got::Frame(t[1]),
            // Written after frame 2, older than it: frame 2 waits for frame 3 to be read.
            s(0, t[1] + 1, b"d", false),
            Got::Frame(t[2]),
            s(0, t[2] + 1, b"e", false),
            Got::Frame(t[3]),
        ]
    );
    assert_eq!(replay.version(), format::VERSION);
    assert_eq!(
        (replay.delivered(), replay.samples(), replay.late_samples()),
        (4, 6, 0)
    );
    assert_eq!(replay.dropped(), 0);
    let channels = replay.channels();
    assert_eq!(channels.len(), 2);
    assert_eq!(channels[0].spec, self::imu());
    assert_eq!(channels[1].spec, self::pose());
    assert!(channels[0].declared.monotonic_ns > 0);
    assert_eq!(replay.channel_named("/pose").map(|c| c.id), Some(pose));
}

/// A sample more than a frame behind (written after frame 2, older than frame 1, which went out
/// once frame 2 was read) comes out late, in file order, and is counted, never dropped.
#[test]
fn a_late_sample_is_counted_not_dropped() {
    let frames = samples(5);
    let t: Vec<u64> = frames.iter().map(|f| f.entry.timestamp_ns).collect();
    let mut recorder = Recorder::new(Vec::new(), &header_for(&frames)).unwrap();
    let imu = recorder.declare(imu()).unwrap();
    for f in &frames[..3] {
        recorder.record_frame(&f.entry, &f.meta, &f.data).unwrap();
    }
    recorder.sample(imu, t[0] + 1, b"old").unwrap();
    let (got, replay) = items(recorder.finish().unwrap());
    assert_eq!(
        got,
        vec![
            Got::Frame(t[0]),
            Got::Frame(t[1]),
            Got::Sample(0, t[0] + 1, b"old".to_vec(), true),
            Got::Frame(t[2]),
        ]
    );
    assert_eq!(replay.late_samples(), 1);
}

/// The documented loop: the samples of a channel between frame n-1 and frame n are the ones
/// that come out between them.
#[test]
fn samples_between_frames_by_the_documented_loop() {
    let frames = samples(9);
    let mut recorder = Recorder::new(Vec::new(), &header_for(&frames)).unwrap();
    let imu = recorder.declare(imu()).unwrap();
    let other = recorder.declare(pose()).unwrap();
    // 1 kHz IMU, written a little before each frame arrives (frames arrive 4 ms late).
    let mut next_imu = frames[0].entry.timestamp_ns - 10_000_000;
    for f in &frames {
        let arrives = f.entry.timestamp_ns + 4_000_000;
        while next_imu <= arrives {
            recorder
                .sample(imu, next_imu, &next_imu.to_le_bytes())
                .unwrap();
            recorder.sample(other, next_imu, b"x").unwrap();
            next_imu += 1_000_000;
        }
        recorder.record_frame(&f.entry, &f.meta, &f.data).unwrap();
    }
    let mut replay = Replay::new(Cursor::new(recorder.finish().unwrap())).unwrap();
    // Declarations at the start are read when the replay opens.
    assert_eq!(replay.channels().len(), 2);
    let imu = replay.channel_named("/imu").map(|c| c.id);
    let mut since_last_frame: Vec<u64> = Vec::new();
    let mut previous: Option<u64> = None;
    let mut checked = 0;
    while let Some(item) = replay.next_item().unwrap() {
        match item {
            Item::Sample(s) if Some(s.channel) == imu && !s.late => {
                since_last_frame.push(s.timestamp_ns);
            }
            Item::Frame(frame) => {
                let ts = frame.timestamp_ns();
                let lower = previous.unwrap_or(0);
                assert!(
                    !since_last_frame.is_empty(),
                    "frame {ts} after {previous:?}"
                );
                assert!(since_last_frame.iter().all(|&s| s > lower && s <= ts));
                // Every IMU sample in (previous, ts] is here, at 1 ms spacing.
                let nearest = *since_last_frame.last().unwrap();
                assert!(ts - nearest < 1_000_000, "nearest gap {}", ts - nearest);
                if previous.is_some() {
                    let expected = (ts - lower).div_ceil(1_000_000);
                    assert!(since_last_frame.len() as u64 >= expected - 1);
                }
                previous = Some(ts);
                since_last_frame.clear();
                checked += 1;
            }
            Item::Sample(_) => {}
        }
    }
    assert_eq!(checked, frames.len());
    assert_eq!(replay.late_samples(), 0);
}

#[test]
fn next_frame_reads_past_samples() {
    let frames = samples(11);
    let mut recorder = Recorder::new(Vec::new(), &header_for(&frames)).unwrap();
    let imu = recorder.declare(imu()).unwrap();
    for f in &frames {
        recorder
            .sample(imu, f.entry.timestamp_ns, &[7; 48])
            .unwrap();
        recorder.record_frame(&f.entry, &f.meta, &f.data).unwrap();
    }
    let mut replay = Replay::new(Cursor::new(recorder.finish().unwrap())).unwrap();
    for f in &frames {
        let lease = replay.next_frame().unwrap().unwrap();
        assert_eq!(*lease.entry(), f.entry);
        assert_eq!(&lease.read().unwrap().bytes()[..f.data.len()], &f.data[..]);
    }
    assert!(replay.next_frame().unwrap().is_none());
    assert_eq!(replay.channels().len(), 1);
}

/// A version 1 file (frames only) reads as before; one with a version-2 record is refused.
#[test]
fn version_one_reads_and_holds_no_channels() {
    let frames = samples(13);
    let mut recorder = Recorder::new(Vec::new(), &header_for(&frames)).unwrap();
    for f in &frames {
        recorder.record_frame(&f.entry, &f.meta, &f.data).unwrap();
    }
    let mut bytes = recorder.finish().unwrap();
    // The version is outside the header's checksum: a v2 recording with no channel records is
    // byte for byte a v1 recording with the version set to 1.
    bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
    let (got, replay) = items(bytes);
    assert_eq!(replay.version(), 1);
    assert_eq!(got.len(), frames.len());
    assert!(got.iter().all(|g| matches!(g, Got::Frame(_))));

    let mut recorder = Recorder::new(Vec::new(), &header_for(&frames)).unwrap();
    recorder.declare(imu()).unwrap();
    let mut bytes = recorder.finish().unwrap();
    bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
    let mut replay = Replay::new(Cursor::new(bytes)).unwrap();
    assert!(matches!(
        replay.next_item(),
        Err(RecordError::Corrupt("unknown record tag"))
    ));
}

#[test]
fn versions_outside_the_range_are_refused_by_number() {
    let frames = samples(15);
    let bytes = Recorder::new(Vec::new(), &header_for(&frames))
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 2);
    for version in [0u16, 3] {
        let mut bad = bytes.clone();
        bad[8..10].copy_from_slice(&version.to_le_bytes());
        assert!(matches!(
            Replay::new(Cursor::new(bad)),
            Err(RecordError::UnsupportedVersion(v)) if v == version
        ));
    }
}

#[test]
fn the_recorder_refuses_bad_channels_and_samples() {
    let frames = samples(17);
    let mut recorder = Recorder::new(Vec::new(), &header_for(&frames)).unwrap();
    let imu_id = recorder.declare(imu()).unwrap();
    assert!(matches!(
        recorder.declare(imu()),
        Err(RecordError::Channel(_))
    ));
    let big = vec![0u8; format::MAX_SAMPLE + 1];
    assert!(matches!(
        recorder.sample(imu_id, 0, &big),
        Err(RecordError::TooLarge("sample"))
    ));
    recorder
        .sample(imu_id, 0, &big[..format::MAX_SAMPLE])
        .unwrap();
    // A channel id from another recording that declared more.
    let mut other = Recorder::new(Vec::new(), &header_for(&frames)).unwrap();
    other.declare(imu()).unwrap();
    let second = other.declare(pose()).unwrap();
    assert!(matches!(
        recorder.sample(second, 0, b""),
        Err(RecordError::Channel(_))
    ));
    let mut spec = pose();
    spec.schema = vec![0; format::MAX_SCHEMA + 1];
    assert!(matches!(
        recorder.declare(spec),
        Err(RecordError::TooLarge("channel schema"))
    ));
}

/// The clocks the importer passes are kept as given.
#[test]
fn declare_at_keeps_the_clocks_given() {
    let frames = samples(19);
    let mut recorder = Recorder::new(Vec::new(), &header_for(&frames)).unwrap();
    let pair = ClockPair {
        monotonic_ns: 11,
        boottime_ns: 13,
    };
    recorder.declare_at(imu(), pair).unwrap();
    let mut replay = Replay::new(Cursor::new(recorder.finish().unwrap())).unwrap();
    assert!(replay.next_item().unwrap().is_none());
    assert_eq!(replay.channels()[0].declared, pair);
}

/// Where the records start: after magic, version, length, header and its checksum.
fn records_start(bytes: &[u8]) -> usize {
    14 + u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize + 4
}

#[test]
fn damaged_channel_records_are_refused() {
    let frames = samples(21);
    let build = || {
        let mut recorder = Recorder::new(Vec::new(), &header_for(&frames)).unwrap();
        let imu = recorder.declare(imu()).unwrap();
        recorder.sample(imu, 5, &[1, 2, 3, 4]).unwrap();
        recorder.finish().unwrap()
    };
    let bytes = build();
    let start = records_start(&bytes);
    let channel_len = u32::from_le_bytes(bytes[start + 1..start + 5].try_into().unwrap()) as usize;
    let sample_at = start + 1 + 4 + channel_len + 4;
    assert_eq!(bytes[sample_at], 3);

    // A byte of the sample's data changed: its checksum fails.
    let mut bad = bytes.clone();
    bad[sample_at + 1 + 14] ^= 0xFF;
    let mut replay = Replay::new(Cursor::new(bad)).unwrap();
    assert!(matches!(
        replay.next_item(),
        Err(RecordError::Corrupt("sample checksum"))
    ));

    // The sample's channel set to one not declared (its checksum recomputed).
    let mut bad = bytes.clone();
    bad[sample_at + 1..sample_at + 3].copy_from_slice(&9u16.to_le_bytes());
    let len = 14 + 4;
    let crc = support::crc32(&bad[sample_at + 1..sample_at + 1 + len]);
    bad[sample_at + 1 + len..sample_at + 1 + len + 4].copy_from_slice(&crc.to_le_bytes());
    let mut replay = Replay::new(Cursor::new(bad)).unwrap();
    assert!(matches!(
        replay.next_item(),
        Err(RecordError::Corrupt("sample of a channel not declared"))
    ));

    // The declaration's id changed (its checksum recomputed): out of order.
    let mut bad = bytes.clone();
    bad[start + 5..start + 7].copy_from_slice(&4u16.to_le_bytes());
    let crc = support::crc32(&bad[start + 5..start + 5 + channel_len]);
    bad[start + 5 + channel_len..start + 9 + channel_len].copy_from_slice(&crc.to_le_bytes());
    // Leading declarations are read at open, so the refusal comes from `new`.
    assert!(matches!(
        Replay::new(Cursor::new(bad)),
        Err(RecordError::Corrupt("channel declared out of order"))
    ));

    // A declaration byte changed: its checksum fails.
    let mut bad = bytes.clone();
    bad[start + 8] ^= 0x01;
    assert!(matches!(
        Replay::new(Cursor::new(bad)),
        Err(RecordError::Corrupt("channel checksum"))
    ));

    // A sample length past the limit.
    let mut bad = bytes.clone();
    bad[sample_at + 11..sample_at + 15]
        .copy_from_slice(&(format::MAX_SAMPLE as u32 + 1).to_le_bytes());
    let mut replay = Replay::new(Cursor::new(bad)).unwrap();
    assert!(matches!(
        replay.next_item(),
        Err(RecordError::TooLarge("sample"))
    ));

    // Cut inside the sample: the replay ends there, truncated.
    let mut cut = bytes;
    cut.truncate(sample_at + 8);
    let mut replay = Replay::new(Cursor::new(cut)).unwrap();
    assert!(replay.next_item().unwrap().is_none());
    assert!(replay.truncated());
}

/// A batch writes the same records as one call per sample, and checks the channel first.
#[test]
fn a_batch_is_the_same_as_its_samples() {
    let frames = samples(23);
    let clocks = ClockPair {
        monotonic_ns: 5,
        boottime_ns: 6,
    };
    let batch: Vec<(u64, Vec<u8>)> = (0..8u64).map(|i| (1_000 + i, vec![i as u8; 24])).collect();
    let mut one = Recorder::new(Vec::new(), &header_for(&frames)).unwrap();
    let id = one.declare_at(imu(), clocks).unwrap();
    for (ts, data) in &batch {
        one.sample(id, *ts, data).unwrap();
    }
    let mut many = Recorder::new(Vec::new(), &header_for(&frames)).unwrap();
    let id = many.declare_at(imu(), clocks).unwrap();
    let written = many
        .sample_batch(id, batch.iter().map(|(ts, d)| (*ts, d.as_slice())))
        .unwrap();
    assert_eq!((written, many.samples()), (8, 8));
    assert_eq!(one.finish().unwrap(), many.finish().unwrap());

    let mut other = Recorder::new(Vec::new(), &header_for(&frames)).unwrap();
    other.declare(imu()).unwrap();
    let second = other.declare(pose()).unwrap();
    let mut rec = Recorder::new(Vec::new(), &header_for(&frames)).unwrap();
    rec.declare(imu()).unwrap();
    assert!(matches!(
        rec.sample_batch(second, [(0u64, &b"x"[..])]),
        Err(RecordError::Channel(_))
    ));
    assert_eq!(rec.samples(), 0);
}
