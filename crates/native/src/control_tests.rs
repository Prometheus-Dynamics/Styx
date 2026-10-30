//! Serving the bridge's requests and the handle's errors; the sensor state itself is tested
//! in `styx-runtime` (`sensor_tests.rs`).

use styx_sensor::{DriverState, MockBus, MockPins, RegWrite, SensorDescription};

use super::*;
use crate::NativeError;

fn desc() -> Arc<SensorDescription> {
    Arc::new(
        SensorDescription::from_file(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../sensor/sensors/ov9782.toml"
        ))
        .unwrap(),
    )
}

type Mock = SensorState<MockBus, MockPins>;

fn control() -> Mock {
    let bus = MockBus::new().with_register(0x300a, 2, 0x9782);
    sensor_control(SensorDriver::new(desc(), bus, MockPins::default()))
}

fn request(action: StreamAction, sequence: u32) -> StreamRequest {
    StreamRequest {
        action,
        sequence,
        timeout: Duration::from_secs(1),
        link_freq: 400_000_000,
        pixel_rate: 160_000_000,
        code: 0x3007,
        width: 1280,
        height: 800,
        hblank: 176,
        vblank: 1022,
        data_lanes: 2,
        continuous_clock: true,
    }
}

const EXPECTED: StartFormat = StartFormat {
    code: 0x3007,
    width: 1280,
    height: 800,
    link_freq: 400_000_000,
};

fn stream_on_writes(c: &Mock) -> usize {
    c.driver()
        .bus()
        .writes()
        .iter()
        .filter(|w| **w == RegWrite::byte(0x0100, 1))
        .count()
}

#[test]
fn bring_up_leaves_the_sensor_in_standby_until_the_start_request() {
    let mut c = control();
    c.bring_up("1280x800", "raw10").unwrap();
    assert_eq!(c.driver().state(), DriverState::Powered);
    assert_eq!(stream_on_writes(&c), 0);
    c.expect_start(EXPECTED);
    c.serve(&request(StreamAction::Start, 1)).unwrap();
    assert_eq!(c.driver().state(), DriverState::Streaming);
    assert_eq!(stream_on_writes(&c), 1);
    assert_eq!(c.starts_served(), 1);
    // Bringing up again while streaming is refused.
    assert!(c.bring_up("1280x800", "raw10").is_err());
    c.serve(&request(StreamAction::Stop, 2)).unwrap();
    assert_eq!(c.driver().state(), DriverState::Powered);
    // A stop without a running stream is fine.
    c.serve(&request(StreamAction::Stop, 3)).unwrap();
    // Switching modes while powered does not power cycle.
    c.bring_up("640x400", "raw10").unwrap();
    assert_eq!(c.driver().mode().unwrap().mode, "640x400");
    c.shut_down().unwrap();
    assert_eq!(c.driver().state(), DriverState::Off);
}

#[test]
fn start_requests_that_do_not_match_are_refused() {
    let mut c = control();
    c.bring_up("1280x800", "raw10").unwrap();
    c.expect_start(EXPECTED);
    let mut req = request(StreamAction::Start, 1);
    req.width = 640;
    assert_eq!(c.serve(&req), Err(libc::EINVAL));
    assert_eq!(stream_on_writes(&c), 0);
    c.driver_mut().bus_mut().fail_writes.insert(0x0100);
    assert_eq!(c.serve(&request(StreamAction::Start, 2)), Err(libc::EIO));
}

#[test]
fn the_handle_reports_errors_as_native_errors() {
    let mut c = control();
    c.bring_up("1280x800", "raw10").unwrap();
    let h = ControlHandle::new(Arc::new(Mutex::new(c)), None);
    assert!(matches!(
        h.set_frame_rate(0.0),
        Err(NativeError::InvalidConfig(_))
    ));
    assert!(matches!(
        h.request_lens_at(3, 100),
        Err(NativeError::InvalidConfig(_))
    ));
    h.set_gain(2.0).unwrap();
    assert!((h.current().unwrap().analog_gain - 2.0).abs() < 1e-9);
    // Immediate writes run on CLOCK_MONOTONIC.
    assert!(lock(h.shared()).clock().is_some());
    assert_eq!(h.frame_time_left(), None);
}

#[test]
fn a_fixed_exposure_lands_on_the_frame_the_handle_predicts() {
    let mut c = control();
    c.bring_up("1280x800", "raw10").unwrap();
    c.expect_start(EXPECTED);
    c.serve(&request(StreamAction::Start, 1)).unwrap();
    let h = ControlHandle::new(Arc::new(Mutex::new(c)), None);
    // Writes made during a frame have time left to land in it (the margin is what the loop
    // uses to decide between writing now and at the next frame start).
    h.set_write_margin(Some(Duration::ZERO));
    let now = lock(h.shared()).now();
    lock(h.shared()).frame_start_at(10, Some(now)).unwrap();

    // Before any frame the value applies from frame 0; during frame 10 it is the frame plus the
    // exposure's delay from the description.
    let delay = u64::from(h.delay(Control::Exposure));
    assert_eq!(h.landing_now(Control::Exposure), 10 + delay);
    assert_eq!(
        h.landing_now(Control::AnalogGain),
        10 + u64::from(h.delay(Control::AnalogGain))
    );

    let exposure = Duration::from_millis(4);
    let req = ControlRequest {
        exposure: Some(exposure),
        gain: None,
        frame_duration: None,
    };
    let landed = h.request_at_now(11, &req).unwrap();
    let exposure_landing = landed
        .iter()
        .find(|l| l.control == Control::Exposure)
        .unwrap()
        .frame;
    assert_eq!(exposure_landing, h.landing_now(Control::Exposure));

    // Frames from the landing on carry the new exposure; the frame before it does not.
    for seq in 11..=exposure_landing {
        lock(h.shared()).frame_start_at(seq, Some(now)).unwrap();
    }
    // The sensor rounds exposure to whole lines (4 ms comes out as 4.004 ms here).
    let near = |d: Duration| d.abs_diff(exposure) < Duration::from_micros(50);
    let before = h.applied(exposure_landing - 1).unwrap();
    let after = h.applied(exposure_landing).unwrap();
    assert!(
        !near(before.exposure),
        "{:?} before the landing",
        before.exposure
    );
    assert!(near(after.exposure), "{:?} at the landing", after.exposure);
}

#[test]
fn a_processed_fixed_exposure_lands_on_the_frame_the_handle_predicts() {
    // The 3A loop's timing as the board measured it (see `landing_processed`): exposure set
    // in frame 10 is taken by the loop's step during frame 11, whose write misses frame 11 (the
    // margin is not left in it), so it is scheduled for the start of frame 12. The step asks for
    // frame 13: its statistics are frame 11's, the controller's issue latency is 0 at 30 fps and
    // the exposure's delay is 2.
    let mut c = control();
    c.bring_up("1280x800", "raw10").unwrap();
    c.expect_start(EXPECTED);
    c.serve(&request(StreamAction::Start, 1)).unwrap();
    let h = ControlHandle::new(Arc::new(Mutex::new(c)), None);
    let now = lock(h.shared()).now();
    let fd = Duration::from_nanos(33_333_333);
    let at = |seq: u64| now + fd * seq as u32;
    lock(h.shared()).frame_start_at(10, Some(at(10))).unwrap();
    let predicted = h.landing_processed(Control::Exposure, None);
    assert_eq!(
        predicted,
        10 + 1 + PROCESSED_LOOP_LATENCY + u64::from(h.delay(Control::Exposure))
    );

    // The step during frame 11: no margin is left in the frame, so the write waits for frame 12.
    h.set_write_margin(None);
    lock(h.shared()).frame_start_at(11, Some(at(11))).unwrap();
    let exposure = Duration::from_millis(4);
    let req = ControlRequest {
        exposure: Some(exposure),
        gain: None,
        frame_duration: None,
    };
    h.request_at_now(13, &req).unwrap();
    for seq in 12..=predicted {
        lock(h.shared()).frame_start_at(seq, Some(at(seq))).unwrap();
    }
    let near = |d: Duration| d.abs_diff(exposure) < Duration::from_micros(50);
    assert!(
        !near(h.applied(predicted - 1).unwrap().exposure),
        "the value shows before the predicted frame {predicted}"
    );
    assert!(
        near(h.applied(predicted).unwrap().exposure),
        "the value does not show on the predicted frame {predicted}"
    );
}

#[test]
fn the_exposure_limit_is_the_frame_less_the_margin_at_the_frame_length_in_effect() {
    // 640x400 at 240 fps (a 4.17 ms frame): the longest exposure is the frame less the sensor's
    // 25-line margin, about 3.9 ms, where 1280x800 at 30 fps allows about its whole frame.
    let mut c = control();
    c.bring_up("640x400", "raw10").unwrap();
    c.expect_start(StartFormat {
        code: 0x3007,
        width: 640,
        height: 400,
        link_freq: 400_000_000,
    });
    let mut req = request(StreamAction::Start, 1);
    req.width = 640;
    req.height = 400;
    c.serve(&req).unwrap();
    let h = ControlHandle::new(Arc::new(Mutex::new(c)), None);
    let now = lock(h.shared()).now();
    let frame = Duration::from_nanos(4_166_667);
    h.set_write_margin(Some(Duration::ZERO));
    h.set_frame_rate(240.0).unwrap();
    for seq in 10..=20u64 {
        lock(h.shared())
            .frame_start_at(seq, Some(now + frame * seq as u32))
            .unwrap();
    }
    let (_, max) = h.exposure_range().unwrap();
    assert!(
        max > Duration::from_micros(3_500) && max < frame,
        "{max:?} at 240 fps"
    );
}
