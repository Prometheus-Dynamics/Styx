//! MCAP export: a `styxrec` recording with frames in several formats and two data channels,
//! exported (lz4 and uncompressed), read back by the `mcap` crate (messages, channels, schemas,
//! summary) and by `import_mcap`, comes back as the same frames (rows packed) and the same
//! channels and samples.

use crate::mcap_import::packed;
use crate::support;

use std::collections::BTreeMap;
use std::io::Cursor;

use styx_controls::FrameMetadata;
use styx_record::{
    ChannelSpec, ClockPair, ExportOptions, Item, McapCompression, Recorder, RecordingHeader,
    Replay, SourceClock, export_mcap, import_mcap,
};

use support::{Sample, fps30, payload_of, samples};

fn imu() -> ChannelSpec {
    ChannelSpec {
        name: "/helios/imu".into(),
        encoding: "json".into(),
        schema_name: "helios.Imu".into(),
        schema_encoding: "jsonschema".into(),
        schema: br#"{"type":"object"}"#.to_vec(),
        clock: SourceClock::Boottime,
        metadata: BTreeMap::from([("source".to_string(), "lemnosd".to_string())]),
    }
}

fn pose() -> ChannelSpec {
    ChannelSpec {
        name: "/pose".into(),
        encoding: "raw".into(),
        ..ChannelSpec::default()
    }
}

/// A sample as written: channel, time, bytes.
type Written = (u16, u64, Vec<u8>);

/// A recording of `frames` with lux and colour temperature set, an IMU sample before and after
/// each frame, a pose sample on each frame's time, and a channel declared at the end.
fn recording(frames: &[Sample]) -> (Vec<u8>, Vec<Written>) {
    let header = RecordingHeader {
        device: "export camera".into(),
        keys: vec!["sensor-1".into(), "serial-42".into()],
        backend: "native".into(),
        format: frames[0].entry.format,
        interval: fps30(),
        payload: payload_of(frames),
    };
    let mut rec = Recorder::new(Vec::new(), &header).unwrap();
    let clocks = ClockPair {
        monotonic_ns: 900,
        boottime_ns: 1_900,
    };
    let imu = rec.declare_at(imu(), clocks).unwrap();
    let pose = rec.declare_at(pose(), clocks).unwrap();
    let mut written = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        let t = f.entry.timestamp_ns;
        let a = format!("{{\"i\":{i},\"a\":1}}").into_bytes();
        rec.sample(imu, t - 1_000, &a).unwrap();
        written.push((imu.get(), t - 1_000, a));
        let mut meta: FrameMetadata = f.meta;
        meta.lux = Some(250.0 + i as f32);
        meta.colour_temperature_k = Some(4_000 + i as u32);
        rec.record_frame(&f.entry, &meta, &f.data).unwrap();
        rec.sample(pose, t, &[i as u8; 48]).unwrap();
        written.push((pose.get(), t, vec![i as u8; 48]));
        let b = format!("{{\"i\":{i},\"b\":2}}").into_bytes();
        rec.sample(imu, t + 1_000, &b).unwrap();
        written.push((imu.get(), t + 1_000, b));
    }
    rec.declare_at(
        ChannelSpec {
            name: "/late".into(),
            ..ChannelSpec::default()
        },
        clocks,
    )
    .unwrap();
    (rec.finish().unwrap(), written)
}

fn export(bytes: &[u8], compression: McapCompression, chunk_size: usize) -> Vec<u8> {
    let mut replay = Replay::new(Cursor::new(bytes.to_vec())).unwrap();
    let options = ExportOptions {
        compression,
        chunk_size,
    };
    let (stats, out) = export_mcap(&mut replay, Vec::new(), options).unwrap();
    assert_eq!(stats.channels, 3);
    assert_eq!(stats.late_samples, 0);
    out
}

fn compressions() -> Vec<McapCompression> {
    let mut all = vec![McapCompression::None];
    if cfg!(feature = "mcap-lz4") {
        all.push(McapCompression::Lz4);
    }
    all
}

#[test]
fn the_mcap_crate_reads_the_export() {
    let frames = samples(51);
    let (bytes, written) = recording(&frames);
    for compression in compressions() {
        for chunk_size in [4096, 1 << 20] {
            let mcap_bytes = export(&bytes, compression, chunk_size);

            // The summary: statistics, channels, schemas and chunk indexes.
            let summary = mcap::Summary::read(&mcap_bytes)
                .unwrap()
                .expect("a summary");
            let stats = summary.stats.as_ref().unwrap();
            // Per frame a meta and an image message; the samples; a clock record per channel.
            let messages = 2 * frames.len() + written.len() + 3;
            assert_eq!(stats.message_count, messages as u64);
            assert!(!summary.chunk_indexes.is_empty());
            let topics: BTreeMap<&str, _> = summary
                .channels
                .values()
                .map(|c| (c.topic.as_str(), c.clone()))
                .collect();
            let imu_channel = &topics["/helios/imu"];
            assert_eq!(imu_channel.message_encoding, "json");
            let schema = imu_channel.schema.as_ref().unwrap();
            assert_eq!(schema.name, "helios.Imu");
            assert_eq!(schema.encoding, "jsonschema");
            assert_eq!(&schema.data[..], br#"{"type":"object"}"#);
            assert_eq!(imu_channel.metadata["source"], "lemnosd");
            assert_eq!(imu_channel.metadata["styx.clock"], "CLOCK_MONOTONIC");
            assert_eq!(imu_channel.metadata["styx.source_clock"], "CLOCK_BOOTTIME");
            assert!(topics["/pose"].schema.is_none());
            assert!(topics.contains_key("/late"));
            assert!(topics.contains_key("/styx/frame_meta"));
            assert!(topics.contains_key("/styx/clock"));

            // The messages, in time order, and each data message as recorded.
            let mut data = Vec::new();
            let mut last = 0;
            let mut per_topic: BTreeMap<String, usize> = BTreeMap::new();
            for message in mcap::MessageStream::new(&mcap_bytes).unwrap() {
                let m = message.unwrap();
                assert!(m.log_time >= last || m.channel.topic == "/styx/clock");
                if m.channel.topic != "/styx/clock" {
                    last = m.log_time;
                }
                assert_eq!(m.log_time, m.publish_time);
                *per_topic.entry(m.channel.topic.clone()).or_default() += 1;
                let id = match m.channel.topic.as_str() {
                    "/helios/imu" => 0,
                    "/pose" => 1,
                    _ => continue,
                };
                data.push((id, m.log_time, m.data.to_vec()));
            }
            let mut expected = written.clone();
            expected.sort_by_key(|(_, t, _)| *t);
            data.sort_by_key(|(_, t, _)| *t);
            assert_eq!(data, expected);
            assert_eq!(per_topic["/styx/frame_meta"], frames.len());
            assert_eq!(per_topic["/styx/clock"], 3);
        }
    }
}

#[test]
fn import_of_the_export_gives_the_recording_back() {
    let frames = samples(53);
    let (bytes, written) = recording(&frames);
    for compression in compressions() {
        let mcap_bytes = export(&bytes, compression, 8192);
        let mut out = Vec::new();
        let stats = import_mcap(&mut Cursor::new(mcap_bytes), &mut out).unwrap();
        assert_eq!(stats.frames, frames.len() as u64);
        assert_eq!(stats.channels, 3);
        assert_eq!(stats.samples, written.len() as u64);
        assert_eq!(stats.clock_records, 3);
        assert_eq!(stats.losses.boottime_assumed_equal, 0);
        assert_eq!(stats.losses.attachments + stats.losses.pyramid_messages, 0);

        let mut original = Replay::new(Cursor::new(bytes.clone())).unwrap();
        let mut back = Replay::new(Cursor::new(out)).unwrap();
        assert_eq!(back.header().device, original.header().device);
        assert_eq!(back.header().keys, original.header().keys);

        let mut samples_back = Vec::new();
        let mut i = 0;
        while let Some(item) = back.next_item().unwrap() {
            match item {
                Item::Frame(lease) => {
                    let f = packed(&frames[i]);
                    assert_eq!(lease.id(), f.entry.id);
                    assert_eq!(lease.timestamp_ns(), f.entry.timestamp_ns);
                    assert_eq!(*lease.entry(), f.entry);
                    let span = f.entry.layouts.span().unwrap();
                    assert_eq!(&lease.read().unwrap().bytes()[..span], &f.data[..]);
                    let m = lease.meta();
                    assert_eq!(m.exposure_us, f.meta.exposure_us);
                    assert_eq!(m.analogue_gain_milli, f.meta.analogue_gain_milli);
                    assert_eq!(m.digital_gain_milli, f.meta.digital_gain_milli);
                    assert_eq!(m.lux, Some(250.0 + i as f32));
                    assert_eq!(m.colour_temperature_k, Some(4_000 + i as u32));
                    i += 1;
                }
                Item::Sample(s) => {
                    assert!(!s.late);
                    samples_back.push((s.channel.get(), s.timestamp_ns, s.data.to_vec()));
                }
            }
        }
        assert_eq!(i, frames.len());
        let mut expected = written.clone();
        expected.sort_by_key(|(_, t, _)| *t);
        samples_back.sort_by_key(|(_, t, _)| *t);
        assert_eq!(samples_back, expected);
        // Channels, once both are read through (the last is declared after the frames): the
        // same specs (the export's styx.* keys dropped) and clocks.
        while original.next_item().unwrap().is_some() {}
        assert_eq!(back.channels().len(), 3);
        for c in original.channels() {
            let b = back.channel_named(&c.spec.name).unwrap();
            assert_eq!(b.spec, c.spec, "channel {}", c.spec.name);
            assert_eq!(b.declared, c.declared);
        }
    }
}

/// Without the `mcap-lz4` feature, an lz4 export is refused by name and nothing is written.
#[cfg(not(feature = "mcap-lz4"))]
#[test]
fn an_lz4_export_is_refused_without_the_feature() {
    let frames = samples(55);
    let (bytes, _) = recording(&frames);
    let mut replay = Replay::new(Cursor::new(bytes)).unwrap();
    let options = ExportOptions {
        compression: McapCompression::Lz4,
        chunk_size: 4096,
    };
    assert!(matches!(
        export_mcap(&mut replay, Vec::new(), options),
        Err(styx_record::RecordError::Unsupported(_))
    ));
    assert_eq!(ExportOptions::default().compression, McapCompression::None);
}
