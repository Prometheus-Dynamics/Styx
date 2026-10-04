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

type Mock = SensorControl<MockBus, MockPins>;

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
