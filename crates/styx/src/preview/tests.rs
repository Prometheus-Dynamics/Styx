use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use styx_core::prelude::*;

use super::worker::RateGate;
use super::*;

fn nv12(w: u32, h: u32, ts: u64) -> FrameLease {
    let (wu, hu) = (w as usize, h as usize);
    let mut bytes: Vec<u8> = (0..wu * hu).map(|i| (i % wu * 255 / wu) as u8).collect();
    bytes.extend(std::iter::repeat_n([100u8, 180], wu / 2 * (hu / 2)).flatten());
    let format = MediaFormat::new(
        FourCc::NV12,
        Resolution::new(w, h).unwrap(),
        ColorSpace::Srgb,
    );
    FrameLease::from_visible_bytes(format, ts, &bytes)
        .unwrap()
        .into_shareable()
}

fn encoders() -> bool {
    !JpegBackend::available().is_empty()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_rate_gate_spreads_frames_evenly() {
    let start = Instant::now();
    let at = |ms: f64| start + Duration::from_secs_f64(ms / 1000.0);
    for (cap, expected) in [(20.0, 20), (15.0, 15), (10.0, 10), (30.0, 30), (0.0, 30)] {
        let mut gate = RateGate::new(cap);
        // 30 fps for a second, with a little jitter.
        let passed = (0..30)
            .filter(|&i| {
                let jitter = if i % 2 == 0 { 0.4 } else { -0.4 };
                gate.due(at(f64::from(i) * 1000.0 / 30.0 + jitter))
            })
            .count();
        assert_eq!(passed, expected, "cap {cap}");
    }
    // After a pause it starts again at once.
    let mut gate = RateGate::new(10.0);
    assert!(gate.due(at(0.0)));
    assert!(!gate.due(at(50.0)));
    assert!(gate.due(at(5000.0)));
    assert!(!gate.due(at(5050.0)));
}

#[test]
fn offered_frames_are_encoded_on_the_preview_thread() {
    if !encoders() {
        return;
    }
    let preview = Preview::new(
        PreviewConfig::new()
            .size(640, 400)
            .max_fps(0.0)
            .name("unit"),
    )
    .unwrap();
    // Nobody watches: nothing is taken.
    assert!(!preview.offer(&nv12(1280, 800, 1)));
    assert_eq!(preview.metrics().dropped_unwatched, 1);
    let mut viewer = preview.subscribe();
    let frame = nv12(1280, 800, 42);
    assert!(preview.offer(&frame));
    let jpeg = viewer
        .recv(Duration::from_secs(10))
        .expect("a preview frame");
    assert_eq!((jpeg.width, jpeg.height, jpeg.sequence), (640, 400, 1));
    assert_eq!(jpeg.timestamp_ns, 42);
    assert_eq!(&jpeg.jpeg[..2], &[0xFF, 0xD8]);
    assert!(!jpeg.gray && !jpeg.passthrough);
    // A frame that owns its buffers cannot be shared: refused, counted.
    let owned = FrameLease::from_visible_bytes(
        MediaFormat::new(
            FourCc::GREY,
            Resolution::new(8, 8).unwrap(),
            ColorSpace::Srgb,
        ),
        3,
        &[0; 64],
    )
    .unwrap();
    assert!(!preview.offer(&owned));
    assert!(preview.offer_owned(owned));
    let grey = viewer.recv(Duration::from_secs(10)).expect("grey frame");
    assert!(grey.gray && grey.width == 8);
    let m = preview.metrics();
    assert_eq!((m.encoded, m.errors), (2, 1), "{m:?}");
    assert!(m.encode.samples >= 2 && m.bytes_per_frame.is_some());
    assert!(m.cpu_ns > 0 || m.encode.samples > 0);
    assert!(
        crate::metrics::snapshot()
            .previews
            .iter()
            .any(|p| p.name == "unit")
    );
    // The original frame is untouched and still ours.
    assert_eq!(frame.meta().timestamp, 42);
}

#[test]
fn only_the_latest_frame_waits_and_viewers_get_the_newest() {
    if !encoders() {
        return;
    }
    let preview = Preview::new(PreviewConfig::new().size(320, 200).max_fps(0.0)).unwrap();
    let mut viewer = preview.subscribe();
    let frames: Vec<FrameLease> = (1..=200).map(|ts| nv12(640, 400, ts)).collect();
    // Offered much faster than they encode: each replaces the one waiting.
    for frame in &frames {
        assert!(preview.offer(frame));
    }
    drop(frames);
    wait_until("the last frame encoded", || {
        preview.latest().is_some_and(|f| f.timestamp_ns == 200)
    });
    let m = preview.metrics();
    assert_eq!(m.frames_in, 200);
    assert_eq!(m.encoded + m.dropped_busy, 200, "{m:?}");
    assert!(m.dropped_busy > 0, "{m:?}");
    // The viewer did not read meanwhile: it gets the newest frame, not a queue.
    let newest = viewer.try_recv().expect("newest");
    assert_eq!((newest.timestamp_ns, newest.sequence), (200, m.encoded));
    assert!(viewer.try_recv().is_none());
}

#[test]
fn frames_beyond_the_cap_are_dropped_before_any_work() {
    if !encoders() {
        return;
    }
    let preview = Preview::new(PreviewConfig::new().size(64, 40).max_fps(2.0)).unwrap();
    let _viewer = preview.subscribe();
    let frame = nv12(128, 80, 1);
    let taken = (0..50).filter(|_| preview.offer(&frame)).count();
    assert_eq!(taken, 1);
    assert_eq!(preview.metrics().dropped_rate, 49);
}

#[test]
fn camera_jpeg_passes_through() {
    if !encoders() {
        return;
    }
    let preview = Preview::new(PreviewConfig::new().max_fps(0.0)).unwrap();
    let mut viewer = preview.subscribe();
    let jpeg = [0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9];
    let format = MediaFormat::new(
        FourCc::MJPG,
        Resolution::new(1280, 720).unwrap(),
        ColorSpace::Srgb,
    );
    let mut buf = BufferPool::with_limits(1, jpeg.len(), 0).lease();
    buf.resize(jpeg.len());
    buf.as_mut_slice().copy_from_slice(&jpeg);
    let frame = FrameLease::single_plane(FrameMeta::new(format, 9), buf, jpeg.len(), jpeg.len());
    assert!(preview.offer_owned(frame));
    let out = viewer.recv(Duration::from_secs(10)).unwrap();
    assert!(out.passthrough);
    assert_eq!(&out.jpeg[..], &jpeg);
    assert_eq!((out.width, out.height), (1280, 720));
}

#[test]
fn websocket_messages_round_trip() {
    let frame = PreviewFrame {
        jpeg: bytes::Bytes::from_static(&[0xFF, 0xD8, 0xFF, 0xD9]),
        sequence: 7,
        timestamp_ns: 123_456_789,
        clock: Some(TimestampClock::Boottime),
        width: 640,
        height: 400,
        gray: true,
        passthrough: false,
        encode_us: 900,
    };
    let message = ws_message(&frame);
    assert_eq!(message.len(), WS_HEADER_LEN + 4);
    assert_eq!(&message[..4], b"SPV1");
    // The layout docs/preview.md documents.
    assert_eq!(&message[4..6], &32u16.to_le_bytes());
    assert_eq!(message[6], 2);
    assert_eq!(message[7], 1);
    assert_eq!(&message[8..16], &7u64.to_le_bytes());
    assert_eq!(&message[16..24], &123_456_789u64.to_le_bytes());
    assert_eq!(&message[24..28], &[128, 2, 144, 1]);
    assert_eq!(&message[28..32], &4u32.to_le_bytes());
    let (header, jpeg) = parse_ws_message(&message).unwrap();
    assert_eq!(jpeg, &frame.jpeg[..]);
    assert_eq!(
        header,
        WsHeader {
            sequence: 7,
            timestamp_ns: 123_456_789,
            clock: Some(TimestampClock::Boottime),
            width: 640,
            height: 400,
            gray: true,
            passthrough: false,
        }
    );
    assert!(parse_ws_message(&message[..message.len() - 1]).is_none());
    assert!(parse_ws_message(b"nope").is_none());
}

#[test]
fn the_mjpeg_body_is_multipart() {
    if !encoders() {
        return;
    }
    let preview = Preview::new(PreviewConfig::new().size(64, 40).max_fps(0.0)).unwrap();
    let mut body = preview.mjpeg();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(body.poll_chunk(&mut cx).is_pending());
    let mut parts = Vec::new();
    for ts in 1..=2 {
        assert!(preview.offer(&nv12(128, 80, ts)));
        wait_until("encoded", || {
            preview.latest().is_some_and(|f| f.timestamp_ns == ts)
        });
        let Poll::Ready(Some(head)) = body.poll_chunk(&mut cx) else {
            panic!("headers");
        };
        let Poll::Ready(Some(jpeg)) = body.poll_chunk(&mut cx) else {
            panic!("jpeg");
        };
        parts.push((String::from_utf8(head.to_vec()).unwrap(), jpeg));
    }
    let (first, jpeg) = &parts[0];
    assert!(
        first.starts_with("--styxpreview\r\nContent-Type: image/jpeg\r\n"),
        "{first}"
    );
    assert!(first.contains(&format!("Content-Length: {}\r\n", jpeg.len())));
    assert!(first.contains("X-Timestamp-Ns: 1\r\n") && first.ends_with("\r\n\r\n"));
    assert!(parts[1].0.starts_with("\r\n--styxpreview\r\n"));
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    assert_eq!(
        MJPEG_CONTENT_TYPE,
        "multipart/x-mixed-replace; boundary=styxpreview"
    );
    preview.stop();
    assert!(matches!(body.poll_chunk(&mut cx), Poll::Ready(None)));
}
