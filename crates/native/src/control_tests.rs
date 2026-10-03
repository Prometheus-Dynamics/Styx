use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use styx_sensor::{MockBus, MockPins, RegWrite};

use super::*;

fn desc() -> Arc<SensorDescription> {
    Arc::new(
        SensorDescription::from_file(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../sensor/sensors/ov9782.toml"
        ))
        .unwrap(),
    )
}

type Mock = SensorControl<MockBus, MockPins>;

fn control() -> Mock {
    let bus = MockBus::new().with_register(0x300a, 2, 0x9782);
    SensorControl::new(SensorDriver::new(desc(), bus, MockPins::default()))
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

const EXPECTED: ExpectedStart = ExpectedStart {
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
fn a_wrong_chip_id_powers_the_sensor_down_again() {
    let bus = MockBus::new().with_register(0x300a, 2, 0x1234);
    let mut c = SensorControl::new(SensorDriver::new(desc(), bus, MockPins::default()));
    assert!(matches!(
        c.bring_up("1280x800", "raw10"),
        Err(NativeError::Sensor(_))
    ));
    assert_eq!(c.driver().state(), DriverState::Off);
}

#[test]
fn requests_before_streaming_apply_from_frame_zero() {
    let mut c = control();
    c.bring_up("1280x800", "raw10").unwrap();
    let t = c.timing().unwrap();
    let thirty = t.frame_length_for_fps(30.0);
    let landed = c
        .request(&ControlRequest {
            frame_duration: Some(thirty.duration),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(landed[0].frame, 0);
    c.serve(&request(StreamAction::Start, 1)).unwrap();
    // Written with the start (before 0x0100 = 1).
    let writes = c.driver().bus().writes();
    let vts = writes
        .iter()
        .position(|w| w.address == 0x380e && w.value == thirty.lines)
        .expect("frame length written");
    let on = writes
        .iter()
        .rposition(|w| *w == RegWrite::byte(0x0100, 1))
        .unwrap();
    assert!(vts < on);
    let a = c.applied(0).unwrap();
    assert_eq!(a.frame_length, thirty.lines);
    assert!((1.0 / a.frame_duration.as_secs_f64() - 30.0).abs() < 0.01);
    assert!(!a.verified);
}

#[test]
fn exposure_lands_after_its_delay_and_frames_report_it() {
    let mut c = control();
    c.bring_up("1280x800", "raw10").unwrap();
    c.serve(&request(StreamAction::Start, 1)).unwrap();
    let before = c.applied(0).unwrap().exposure;
    c.frame_start(0).unwrap();
    c.frame_start(1).unwrap();
    let target = before * 2;
    let landed = c
        .request(&ControlRequest {
            exposure: Some(target),
            ..Default::default()
        })
        .unwrap();
    // Requested for frame 2; the exposure's two-frame delay puts it on frame 4.
    assert_eq!(c.next_frame(), 2);
    assert_eq!(landed[0].control, Control::Exposure);
    assert_eq!(landed[0].frame, 4);
    // Nothing is written until the next frame start, then inside group hold.
    let n = c.driver().bus().writes().len();
    assert!(c.frame_start(1).unwrap().is_empty(), "repeats are ignored");
    let written = c.frame_start(2).unwrap();
    assert!(written.get(Control::Exposure).is_some());
    let w = c.driver().bus().writes();
    assert_eq!(w[n], RegWrite::byte(0x3208, 0x00));
    assert_eq!(*w.last().unwrap(), RegWrite::byte(0x3208, 0xa0));
    let lines = |f| c.applied(f).unwrap().exposure_lines;
    assert_eq!(lines(3), lines(0));
    assert!((lines(4) - 2.0 * lines(0)).abs() <= 1.0);
    assert_eq!(c.latest().unwrap().exposure, c.applied(2).unwrap().exposure);
    assert_eq!(c.frame_starts(), 3);
}

#[test]
fn invalid_requests_are_refused() {
    let mut c = control();
    c.bring_up("1280x800", "raw10").unwrap();
    for req in [
        ControlRequest {
            frame_duration: Some(Duration::ZERO),
            ..Default::default()
        },
        ControlRequest {
            gain: Some(f64::NAN),
            ..Default::default()
        },
        ControlRequest {
            gain: Some(-1.0),
            ..Default::default()
        },
    ] {
        assert!(c.request(&req).is_err(), "{req:?}");
    }
    // Without embedded data layout nothing is reported.
    assert!(c.report_embedded(0, &[1, 2, 3]).unwrap().is_empty());
}

#[test]
fn the_handle_converts_rates_and_reports_ranges() {
    let mut c = control();
    c.bring_up("1280x800", "raw10").unwrap();
    let vblank = Arc::new(AtomicU32::new(0));
    let seen = Arc::clone(&vblank);
    let hook: BlankingHook = Arc::new(move |_, v| seen.store(v, Ordering::Relaxed));
    let h = ControlHandle::new(Arc::new(Mutex::new(c)), Some(hook));
    h.set_frame_rate(30.0).unwrap();
    assert_eq!(vblank.load(Ordering::Relaxed), 3662 - 800);
    assert!(h.set_frame_rate(0.0).is_err());
    let (lo, hi) = h.fps_range().unwrap();
    assert!(lo < 2.2 && hi > 120.0);
    assert_eq!(h.gain_range(), (1.0, 255.0 / 16.0));
    let (emin, emax) = h.exposure_range().unwrap();
    assert!(emin < emax);
    assert_eq!(h.delay(Control::Exposure), 2);
    h.set_gain(2.0).unwrap();
    h.set_exposure(Duration::from_millis(5)).unwrap();
    // Before streaming, frame 0 carries the requests.
    let now = h.current().unwrap();
    assert!((now.analog_gain - 2.0).abs() < 1e-9);
    assert!((now.exposure.as_secs_f64() - 0.005).abs() < 1e-4);
    assert_eq!(now.gain(), 2.0);
    assert!(h.applied(0).is_some());
    let _ = format!("{h:?}");
    let _clone = h.clone();
}

#[test]
fn requests_now_are_written_within_the_frame_when_time_is_left() {
    let mut c = control();
    c.bring_up("1280x800", "raw10").unwrap();
    // Before streaming: written at once, applying from frame 0.
    let n = c.driver().bus().writes().len();
    let short = ControlRequest {
        exposure: Some(Duration::from_millis(2)),
        ..Default::default()
    };
    let landed = c.request_at_now(0, &short, Duration::ZERO).unwrap();
    assert_eq!(landed[0].frame, 0);
    assert!(c.driver().bus().writes().len() > n);
    c.serve(&request(StreamAction::Start, 1)).unwrap();
    // Frame 5 started at t = 1 s; a frame lasts about 33 ms at the default frame length.
    let t0 = Duration::from_secs(1);
    c.frame_start_at(5, Some(t0)).unwrap();
    let fd = c.applied(5).unwrap().frame_duration;
    let long = ControlRequest {
        exposure: Some(Duration::from_millis(4)),
        ..Default::default()
    };
    // 10 ms into the frame: written now (inside group hold), lands on 5 + 2.
    let n = c.driver().bus().writes().len();
    let landed = c
        .request_at_now(7, &long, t0 + Duration::from_millis(10))
        .unwrap();
    assert_eq!(landed[0].frame, 7);
    assert!(!c.writes_pending(), "everything due was written");
    let w = c.driver().bus().writes();
    assert_eq!(w[n], RegWrite::byte(0x3208, 0x00));
    assert_eq!(*w.last().unwrap(), RegWrite::byte(0x3208, 0xa0));
    assert_eq!(
        c.applied(7).unwrap().exposure,
        c.applied(100).unwrap().exposure
    );
    // Too close to the frame's end: left for the next frame start, landing a frame later.
    let n = c.driver().bus().writes().len();
    let landed = c
        .request_at_now(7, &short, t0 + fd - Duration::from_millis(1))
        .unwrap();
    assert_eq!(landed[0].frame, 8);
    assert_eq!(c.driver().bus().writes().len(), n);
    // It waits for the next frame start (a caller driving frame starts must wait for one).
    assert!(c.writes_pending());
    // Without a frame start time (frames inferred from dequeues) nothing is written early.
    c.frame_start(6).unwrap();
    assert_eq!(c.frame_time_left(t0), None);
    c.set_write_margin(None);
    c.frame_start_at(7, Some(t0 + fd * 2)).unwrap();
    let landed = c.request_at_now(9, &long, t0 + fd * 2).unwrap();
    assert_eq!(landed[0].frame, 10);
}

#[test]
fn a_powered_sensor_in_the_same_mode_is_not_set_up_again() {
    let mut c = control();
    c.bring_up("1280x800", "raw10").unwrap();
    let n = c.driver().bus().writes().len();
    c.bring_up("1280x800", "raw10").unwrap();
    assert_eq!(c.driver().bus().writes().len(), n, "nothing rewritten");
    assert_eq!(c.bring_up_times().power_up, Duration::ZERO);
    // Another mode is written.
    c.bring_up("1280x720", "raw10").unwrap();
    assert!(c.driver().bus().writes().len() > n);
}

#[test]
fn bring_up_stops_a_sensor_left_streaming_before_init() {
    // A previous owner killed mid-stream leaves the sensor streaming when its digital rails
    // stay on: bring-up writes stream_off before the init registers.
    let mut c = control();
    c.bring_up("1280x800", "raw10").unwrap();
    let writes = c.driver().bus().writes();
    let off = writes
        .iter()
        .position(|w| *w == RegWrite::byte(0x0100, 0))
        .expect("stream_off written");
    let init = writes
        .iter()
        .position(|w| *w == RegWrite::byte(0x0302, 0x32))
        .expect("init written");
    assert!(off < init, "stream_off at {off}, init at {init}");
}
