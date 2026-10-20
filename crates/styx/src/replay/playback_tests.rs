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

/// A recording of the PiSP's raw frames (16-bit BGGR) replays processed: NV12 frames that keep
/// the recorded metadata, 3A record included.
#[cfg(feature = "replay-softisp")]
#[test]
fn raw_recordings_replay_through_the_software_isp() {
    let path = temp_path("raw-isp.mcap");
    let (w, h) = (640u32, 400u32);
    let format = MediaFormat::new(
        FourCc::new(*b"BYR2"),
        Resolution::new(w, h).unwrap(),
        ColorSpace::Unknown,
    );
    let aaa = NativeAaa {
        colour_gains: [1.6, 1.0, 1.9],
        colour_temperature_k: 4000.0,
        lux: 300.0,
        ae_locked: true,
        awb_converged: true,
        flicker_period_ns: 0,
        lens: None,
    };
    let mut rec = StreamRecorder::with_header(&path, &header(format)).unwrap();
    for i in 0..3u32 {
        // A mid grey of the 10-bit sensor, in the top bits.
        let bytes: Vec<u8> = (0..w * h)
            .flat_map(|_| (300u16 << 6).to_le_bytes())
            .collect();
        let mut f =
            FrameLease::from_visible_bytes(format, u64::from(i) * 33_000_000, &bytes).unwrap();
        f.meta_mut().backend = Some(BackendFrameMeta::Native(NativeFrameMeta {
            sequence: i,
            exposure_ns: 8_000_000,
            analog_gain: 2.0,
            digital_gain: 1.0,
            frame_duration_ns: 33_333_333,
            aaa: Some(aaa),
            ..Default::default()
        }));
        rec.record(&f).unwrap();
    }
    rec.finish().unwrap();
    let source = CaptureRequest::replay_source(
        ReplaySourceConfig::new(&path)
            .pacing(ReplayPacing::Unpaced)
            .soft_isp("ov9782"),
    )
    .unwrap();
    assert_eq!(
        source.device().backends[0].descriptor.modes[0].format.code,
        FourCc::NV12
    );
    let handle = source.open().unwrap();
    let f = next(&handle);
    let meta = f.meta();
    assert_eq!(meta.format.code, FourCc::NV12);
    assert_eq!(meta.format.resolution, Resolution::new(w, h).unwrap());
    assert_eq!(meta.timestamp, 0);
    let native = meta.native().copied().unwrap();
    assert_eq!((native.exposure_ns, native.aaa), (8_000_000, Some(aaa)));
    // A picture, not black: the grey comes through.
    let y = f.planes()[0].data()[(w * h / 2 + w / 2) as usize];
    assert!(y > 20, "luma {y}");
    handle.stop();
    let _ = std::fs::remove_file(path);
}
