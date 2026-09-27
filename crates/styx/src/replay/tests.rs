use std::path::PathBuf;
use std::time::{Duration, Instant};

use styx_core::prelude::*;

use super::*;
use crate::capture_api::CaptureRequest;
use crate::{BackendKind, DeviceIdentity};

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("styx-replay-{}-{name}.styxrec", std::process::id()))
}

fn header(format: MediaFormat) -> RecordingHeader {
    RecordingHeader {
        device: DeviceIdentity {
            display: "test camera".into(),
            keys: vec!["test:1".into()],
        },
        backend: "libcamera".into(),
        format,
        interval: Interval::from_fps(50),
    }
}

fn grey(width: u32, height: u32) -> MediaFormat {
    MediaFormat::new(
        FourCc::GREY,
        Resolution::new(width, height).unwrap(),
        ColorSpace::Unknown,
    )
}

/// A GREY frame whose pixels encode its index, with metadata set the way a live capture would.
fn frame(index: u32, timestamp: u64) -> FrameLease {
    let format = grey(64, 32);
    let bytes: Vec<u8> = (0..64 * 32).map(|i| (i as u32 + index) as u8).collect();
    let mut frame = FrameLease::from_visible_bytes(format, timestamp, &bytes).unwrap();
    let meta = frame.meta_mut();
    meta.clock = Some(TimestampClock::Boottime);
    meta.backend = Some(BackendFrameMeta::Libcamera(LibcameraFrameMeta {
        sequence: 100 + index,
        buffer_memory: "dma-heap",
    }));
    meta.timing.sensor_to_capture = Some(Duration::from_micros(8_200));
    frame
}

fn record(path: &PathBuf, frames: impl IntoIterator<Item = FrameLease>) {
    let mut recorder = StreamRecorder::with_header(path, &header(grey(64, 32))).unwrap();
    for frame in frames {
        recorder.record(&frame).unwrap();
    }
    recorder.finish().unwrap();
}

#[test]
fn frames_round_trip_with_metadata_crop_companions_and_bitstreams() {
    let path = temp_path("round-trip");
    let with_pyramid = frame(1, 1_000)
        .crop_view(FrameRect::new(8, 4, 32, 16))
        .unwrap()
        .with_box_pyramid(1, 1)
        .unwrap();
    let mjpeg_format = MediaFormat::new(
        FourCc::MJPG,
        Resolution::new(64, 32).unwrap(),
        ColorSpace::Srgb,
    );
    let jpeg = [0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9];
    let mut pool_buffer = BufferPool::with_limits(1, 64, 0).lease();
    pool_buffer.resize(jpeg.len());
    pool_buffer.as_mut_slice().copy_from_slice(&jpeg);
    let mjpeg = FrameLease::single_plane(
        FrameMeta::new(mjpeg_format, 2_000),
        pool_buffer,
        jpeg.len(),
        jpeg.len(),
    );
    let originals = [frame(0, 0), with_pyramid, mjpeg];
    let mut recorder = StreamRecorder::with_header(&path, &header(grey(64, 32))).unwrap();
    for f in &originals {
        recorder.record(f).unwrap();
    }
    recorder.finish().unwrap();

    let (read_header, frames) = open_recording(&path).unwrap();
    assert_eq!(read_header.device.keys, vec!["test:1".to_string()]);
    assert_eq!(read_header.interval, Interval::from_fps(50));
    let replayed: Vec<FrameLease> = frames.map(Result::unwrap).collect();
    assert_eq!(replayed.len(), 3);
    for (a, b) in originals.iter().zip(&replayed) {
        let (ma, mb) = (a.meta(), b.meta());
        assert_eq!(ma.format, mb.format);
        assert_eq!(ma.timestamp, mb.timestamp);
        assert_eq!(ma.clock, mb.clock);
        assert_eq!(ma.backend, mb.backend);
        assert_eq!(ma.crop, mb.crop);
        assert_eq!(ma.timing, mb.timing);
    }
    assert_eq!(
        originals[0].to_visible_vec().unwrap(),
        replayed[0].to_visible_vec().unwrap()
    );
    let level = replayed[1].pyramid_level(1).expect("companion");
    let original_level = originals[1].pyramid_level(1).unwrap();
    assert_eq!(
        level.to_visible_vec().unwrap(),
        original_level.to_visible_vec().unwrap()
    );
    assert_eq!(level.meta().crop, original_level.meta().crop);
    assert_eq!(replayed[2].planes()[0].data(), &jpeg);
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_recording_cut_short_replays_its_complete_frames() {
    let path = temp_path("truncated");
    record(&path, (0..3).map(|i| frame(i, u64::from(i) * 1_000)));
    let len = std::fs::metadata(&path).unwrap().len();
    // Drop the end marker and half of the last frame.
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(len - 1 - 1_000).unwrap();
    let frames: Vec<_> = open_recording(&path).unwrap().1.collect();
    assert_eq!(frames.len(), 2);
    assert!(frames.iter().all(Result::is_ok));
    let _ = std::fs::remove_file(path);
}

#[test]
fn unpaced_replay_delivers_every_frame_then_closes() {
    let path = temp_path("unpaced");
    record(
        &path,
        (0..10).map(|i| frame(i, 5_000_000 + u64::from(i) * 20_000_000)),
    );
    let source =
        CaptureRequest::replay_source(ReplaySourceConfig::new(&path).pacing(ReplayPacing::Unpaced))
            .unwrap();
    let device = source.device().clone();
    assert_eq!(device.backends[0].kind, BackendKind::Replay);
    let handle = source.open().unwrap();
    let mut timestamps = Vec::new();
    loop {
        match handle.recv_blocking(Duration::from_secs(2)) {
            RecvOutcome::Data(frame) => {
                assert_eq!(frame.meta().clock, Some(TimestampClock::Boottime));
                timestamps.push(frame.meta().timestamp);
            }
            RecvOutcome::Closed => break,
            RecvOutcome::Empty => panic!("replay stalled"),
        }
    }
    let expected: Vec<u64> = (0..10).map(|i| 5_000_000 + i * 20_000_000).collect();
    assert_eq!(timestamps, expected);
    handle.stop();
    let _ = std::fs::remove_file(path);
}

#[test]
fn looped_replay_keeps_timestamps_increasing() {
    let path = temp_path("loop");
    record(&path, (0..4).map(|i| frame(i, u64::from(i) * 20_000_000)));
    let handle = CaptureRequest::replay_source(
        ReplaySourceConfig::new(&path)
            .pacing(ReplayPacing::Unpaced)
            .loop_forever(true),
    )
    .unwrap()
    .open()
    .unwrap();
    let mut timestamps = Vec::new();
    while timestamps.len() < 10 {
        if let RecvOutcome::Data(frame) = handle.recv_blocking(Duration::from_secs(2)) {
            timestamps.push(frame.meta().timestamp);
        }
    }
    handle.stop();
    // Loops continue one frame interval (20 ms) after the last frame.
    let expected: Vec<u64> = (0..10).map(|i| i * 20_000_000).collect();
    assert_eq!(timestamps, expected);
    let _ = std::fs::remove_file(path);
}

#[test]
fn realtime_replay_follows_recorded_timing() {
    let path = temp_path("realtime");
    record(&path, (0..6).map(|i| frame(i, u64::from(i) * 20_000_000)));
    let handle = CaptureRequest::replay_source(ReplaySourceConfig::new(&path))
        .unwrap()
        .open()
        .unwrap();
    let mut first = None;
    let mut received = 0;
    let mut last = Instant::now();
    loop {
        match handle.recv_blocking(Duration::from_secs(2)) {
            RecvOutcome::Data(_) => {
                received += 1;
                last = Instant::now();
                first.get_or_insert(last);
            }
            RecvOutcome::Closed => break,
            RecvOutcome::Empty => panic!("replay stalled"),
        }
    }
    handle.stop();
    assert_eq!(received, 6);
    let span = last - first.unwrap();
    assert!(span >= Duration::from_millis(90), "{span:?}");
    let _ = std::fs::remove_file(path);
}

#[test]
fn foreign_files_are_rejected() {
    let path = temp_path("foreign");
    std::fs::write(&path, b"not a recording at all").unwrap();
    assert!(matches!(
        read_header(&path),
        Err(ReplayError::NotARecording)
    ));
    let _ = std::fs::remove_file(path);
}
