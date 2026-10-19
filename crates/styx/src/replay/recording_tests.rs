//! `StreamRecorder::mcap`: the writer thread, drops, host channels, metadata, attachments, the
//! clock record and chunk compression, read back with the mcap crate and with Styx's replay.

use std::collections::BTreeMap;
use std::time::Duration;

use super::tests::{frame, grey, header, temp_path};
use super::*;

/// Every message's topic, in file order, and the summary.
fn read(path: &std::path::Path) -> (Vec<(String, Vec<u8>)>, mcap::Summary) {
    let bytes = std::fs::read(path).unwrap();
    let msgs = mcap::MessageStream::new(&bytes)
        .unwrap()
        .map(|m| {
            let m = m.unwrap();
            (m.channel.topic.clone(), m.data.into_owned())
        })
        .collect();
    let summary = mcap::Summary::read(&bytes).unwrap().expect("a summary");
    (msgs, summary)
}

fn replayed(path: &std::path::Path) -> Vec<u64> {
    open_recording(path)
        .unwrap()
        .1
        .map(|f| f.unwrap().meta().timestamp)
        .collect()
}

#[test]
fn host_channels_metadata_attachments_and_clock_go_with_the_frames() {
    let path = temp_path("host-channels.mcap");
    let mut rec =
        StreamRecorder::mcap(&path, &header(grey(64, 32)), RecorderOptions::default()).unwrap();
    let imu = rec
        .add_channel(
            "/robot/imu",
            Some(ChannelSchema {
                name: "Imu".into(),
                encoding: "jsonschema".into(),
                data: br#"{"type":"object"}"#.to_vec(),
            }),
            "json",
            BTreeMap::new(),
        )
        .unwrap();
    let poses = rec
        .add_channel("/robot/poses", None, "json", BTreeMap::new())
        .unwrap();
    rec.add_metadata(
        "camera_mount",
        BTreeMap::from([("x_m".into(), "0.1".into())]),
    )
    .unwrap();
    rec.add_attachment("field.json", "application/json", 5, b"{}".to_vec())
        .unwrap();
    for i in 0..4u32 {
        rec.record(&frame(i, u64::from(i) * 1_000)).unwrap();
        rec.write(imu, u64::from(i) * 1_000 + 1, br#"{"ax":0}"#)
            .unwrap();
    }
    rec.write(poses, 9_000, br#"[]"#).unwrap();
    let stats = rec.stats();
    assert_eq!(stats.frames_dropped + stats.messages_dropped, 0);
    rec.finish().unwrap();

    let (msgs, summary) = read(&path);
    let count = |topic: &str| msgs.iter().filter(|(t, _)| t == topic).count();
    assert_eq!(count("/styx/image"), 4);
    assert_eq!(count("/styx/frame_meta"), 4);
    assert_eq!(count("/robot/imu"), 4);
    assert_eq!(count("/robot/poses"), 1);
    // One clock sample at the start and one at the end, at least.
    assert!(count("/styx/clock") >= 2);
    let imu_schema = summary
        .channels
        .values()
        .find(|c| c.topic == "/robot/imu")
        .and_then(|c| c.schema.clone())
        .expect("schema");
    assert_eq!(imu_schema.encoding, "jsonschema");
    assert_eq!(summary.attachment_indexes.len(), 1);
    assert_eq!(summary.attachment_indexes[0].name, "field.json");
    assert!(
        summary
            .metadata_indexes
            .iter()
            .any(|m| m.name == "camera_mount")
    );
    // Replay reads the frames and skips the host's and the clock's messages.
    assert_eq!(replayed(&path), vec![0, 1_000, 2_000, 3_000]);
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_full_queue_drops_and_counts_frames_and_host_messages() {
    let path = temp_path("drops.mcap");
    let options = RecorderOptions {
        // Smaller than one frame: every frame and message is dropped.
        queue_bytes: 16,
        clock_period: None,
        ..Default::default()
    };
    let mut rec = StreamRecorder::mcap(&path, &header(grey(64, 32)), options).unwrap();
    let ch = rec
        .add_channel("/robot/power", None, "json", BTreeMap::new())
        .unwrap();
    for i in 0..3u32 {
        rec.record(&frame(i, u64::from(i))).unwrap();
    }
    rec.write(ch, 1, &[0u8; 64]).unwrap();
    let stats = rec.stats();
    assert_eq!((stats.frames_dropped, stats.messages_dropped), (3, 1));
    rec.finish().unwrap();
    let (msgs, _) = read(&path);
    assert!(msgs.is_empty(), "{} messages", msgs.len());
}

#[test]
fn without_a_writer_thread_every_call_writes() {
    let path = temp_path("sync.mcap");
    let options = RecorderOptions {
        queue_bytes: 0,
        clock_period: Some(Duration::from_secs(60)),
        ..Default::default()
    };
    let mut rec = StreamRecorder::mcap(&path, &header(grey(64, 32)), options).unwrap();
    rec.record(&frame(0, 10)).unwrap();
    let stats = rec.stats();
    assert_eq!(stats.frames_written, 1);
    assert!(stats.bytes_written > 64 * 32);
    rec.finish().unwrap();
    assert_eq!(replayed(&path), vec![10]);
}

#[test]
fn compression_without_its_feature_is_refused() {
    let path = temp_path("refused.mcap");
    for compression in [RecordCompression::Lz4, RecordCompression::Zstd { level: 1 }] {
        let enabled = match compression {
            RecordCompression::Lz4 => cfg!(feature = "replay-mcap-lz4"),
            _ => cfg!(feature = "replay-mcap-zstd"),
        };
        let options = RecorderOptions {
            compression,
            ..Default::default()
        };
        let made = StreamRecorder::mcap(&path, &header(grey(64, 32)), options);
        assert_eq!(made.is_ok(), enabled, "{compression:?}");
        if let Ok(rec) = made {
            rec.finish().unwrap();
        }
    }
    let _ = std::fs::remove_file(path);
}

/// Compressed recordings replay frame for frame.
#[cfg(any(feature = "replay-mcap-lz4", feature = "replay-mcap-zstd"))]
#[test]
fn compressed_recordings_replay() {
    let codecs = [
        #[cfg(feature = "replay-mcap-lz4")]
        RecordCompression::Lz4,
        #[cfg(feature = "replay-mcap-zstd")]
        RecordCompression::Zstd { level: 3 },
    ];
    for compression in codecs {
        let path = temp_path(&format!("compressed-{compression:?}.mcap"));
        let options = RecorderOptions {
            compression,
            chunk_size: Some(16 << 10),
            ..Default::default()
        };
        let mut rec = StreamRecorder::mcap(&path, &header(grey(64, 32)), options).unwrap();
        let ch = rec
            .add_channel("/robot/imu", None, "json", BTreeMap::new())
            .unwrap();
        for i in 0..20u32 {
            rec.record(&frame(i, u64::from(i) * 100)).unwrap();
            rec.write(ch, u64::from(i) * 100, b"{}").unwrap();
        }
        rec.finish().unwrap();
        let frames: Vec<FrameLease> = open_recording(&path)
            .unwrap()
            .1
            .map(Result::unwrap)
            .collect();
        assert_eq!(frames.len(), 20, "{compression:?}");
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.meta().timestamp, i as u64 * 100);
            let want = frame(i as u32, 0);
            assert_eq!(
                f.to_visible_vec().unwrap(),
                want.to_visible_vec().unwrap(),
                "{compression:?} frame {i}"
            );
        }
        let _ = std::fs::remove_file(path);
    }
}
