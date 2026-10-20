//! A replay capture's playback controls: pause, seek, step, speed, hold at the end, length,
//! rebased timestamps.

use std::time::{Duration, Instant};

use super::tests::{frame, grey, header, temp_path};
use super::*;
use crate::capture_api::CaptureRequest;
use crate::capture_api::replay_controls::ids;

/// Ten frames 20 ms apart.
fn recording(name: &str) -> PathBuf {
    let path = temp_path(name);
    let mut rec = StreamRecorder::with_header(&path, &header(grey(64, 32))).unwrap();
    for i in 0..10u32 {
        rec.record(&frame(i, u64::from(i) * 20_000_000)).unwrap();
    }
    rec.finish().unwrap();
    path
}

fn next(handle: &crate::capture_api::CaptureHandle) -> FrameLease {
    match handle.recv_blocking(Duration::from_secs(2)) {
        RecvOutcome::Data(f) => f,
        RecvOutcome::Empty => panic!("no frame"),
        RecvOutcome::Closed => panic!("closed"),
    }
}

fn quiet(handle: &crate::capture_api::CaptureHandle, for_ms: u64) -> bool {
    matches!(
        handle.recv_blocking(Duration::from_millis(for_ms)),
        RecvOutcome::Empty
    )
}

#[test]
fn pause_seek_step_and_hold_at_the_end() {
    let path = recording("playback.mcap");
    let handle = CaptureRequest::replay_source(
        ReplaySourceConfig::new(&path)
            .pacing(ReplayPacing::Unpaced)
            .hold_at_end(true),
    )
    .unwrap()
    .open()
    .unwrap();
    // The controls are listed.
    assert!(
        handle
            .control_metas()
            .iter()
            .any(|m| m.id == ids::REPLAY_SEEK_MS)
    );
    handle
        .set_control(ids::REPLAY_PLAYING, ControlValue::Bool(false))
        .unwrap();
    // Frames delivered before the pause drain; then nothing comes.
    while !quiet(&handle, 150) {}
    // A seek while paused shows the frame there, and only it.
    handle
        .set_control(ids::REPLAY_SEEK_MS, ControlValue::Uint(100))
        .unwrap();
    assert_eq!(next(&handle).meta().timestamp, 100_000_000);
    assert!(quiet(&handle, 150));
    assert_eq!(
        handle.get_control(ids::REPLAY_FRAME).unwrap(),
        ControlValue::Uint(5)
    );
    assert_eq!(
        handle.get_control(ids::REPLAY_STATE).unwrap(),
        ControlValue::Int(1)
    );
    // Step: one frame, still paused.
    handle
        .set_control(ids::REPLAY_STEP, ControlValue::Bool(true))
        .unwrap();
    assert_eq!(next(&handle).meta().timestamp, 120_000_000);
    assert!(quiet(&handle, 150));
    // By frame.
    handle
        .set_control(ids::REPLAY_SEEK_FRAME, ControlValue::Uint(2))
        .unwrap();
    assert_eq!(next(&handle).meta().timestamp, 40_000_000);
    // Play to the end: held there (no `Closed`), ended.
    handle
        .set_control(ids::REPLAY_PLAYING, ControlValue::Bool(true))
        .unwrap();
    let mut last = 0;
    while let RecvOutcome::Data(f) = handle.recv_blocking(Duration::from_millis(300)) {
        last = f.meta().timestamp;
    }
    assert_eq!(last, 180_000_000);
    assert_eq!(
        handle.get_control(ids::REPLAY_STATE).unwrap(),
        ControlValue::Int(2)
    );
    // The length was found meanwhile.
    assert_eq!(
        handle.get_control(ids::REPLAY_FRAME_COUNT).unwrap(),
        ControlValue::Uint(10)
    );
    assert_eq!(
        handle.get_control(ids::REPLAY_DURATION_MS).unwrap(),
        ControlValue::Uint(180)
    );
    // A seek after the end plays on from there.
    handle
        .set_control(ids::REPLAY_SEEK_FRAME, ControlValue::Uint(8))
        .unwrap();
    assert_eq!(next(&handle).meta().timestamp, 160_000_000);
    assert!(
        handle
            .set_control(ids::REPLAY_FRAME, ControlValue::Uint(1))
            .is_err()
    );
    assert!(
        handle
            .set_control(ids::REPLAY_SPEED, ControlValue::Float(9.0))
            .is_err()
    );
    handle.stop();
    let _ = std::fs::remove_file(path);
}

#[test]
fn speed_paces_and_rebased_timestamps_are_delivery_times() {
    let path = recording("speed.mcap");
    let handle = CaptureRequest::replay_source(
        ReplaySourceConfig::new(&path).timestamps(ReplayTimestamps::Rebased),
    )
    .unwrap()
    .open()
    .unwrap();
    handle
        .set_control(ids::REPLAY_SPEED, ControlValue::Float(4.0))
        .unwrap();
    let start = Instant::now();
    let mut stamps = Vec::new();
    while let RecvOutcome::Data(f) = handle.recv_blocking(Duration::from_secs(2)) {
        assert_eq!(f.meta().clock, Some(TimestampClock::Monotonic));
        stamps.push(f.meta().timestamp);
    }
    // 180 ms of recording at 4x: about 45 ms (the first frame may have gone at 1x).
    assert_eq!(stamps.len(), 10);
    assert!(
        start.elapsed() < Duration::from_millis(170),
        "{:?}",
        start.elapsed()
    );
    // Delivery times: increasing, on the monotonic clock of this process.
    assert!(stamps.windows(2).all(|w| w[1] >= w[0]));
    let now = TimestampClock::Monotonic.now_ns().unwrap();
    assert!(now >= stamps[9] && now - stamps[9] < 2_000_000_000);
    let _ = std::fs::remove_file(path);
}
