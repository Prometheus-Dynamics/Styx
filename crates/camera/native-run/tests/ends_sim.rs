//! The two stream ends a session cannot cause itself, provoked on the simulated backend: a
//! receiver the host turns off for good ends the stream as device-failed, and a receiver that
//! ends its consumer for breaking the rules ends it as a consumer fault. Each comes back from the
//! session as `Error::StreamEnded` with its kind, its words, and the session stopped.
//!
//! Both cases are in this one test because the simulator's switches are process-wide and a test
//! binary runs its tests in parallel.

use styx_sim::paced as sim;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use lemnos_contract_camera_receiver::BayerOrder;
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_lemnos::{Client, StreamEnd};
use styx_native_run::{Error, Event, Initial, Policy, Request, Session, State, SystemClock};

const CONTROLS: Initial = Initial {
    exposure_us: 8_000,
    gain_milli: 1_000,
    frame_rate_milli: 60_000,
};

const REQUEST: Request = Request {
    width: 320,
    height: 40,
    fps_milli: 60_000,
    bit_depth: Some(10),
    format: None,
    bayer: Some(BayerOrder::Rggb),
};

fn start(name: &str, text: &str) -> (Service, PathBuf) {
    let dir = std::env::temp_dir()
        .join(format!("styx-native-run-ends-{}", std::process::id()))
        .join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("sock");
    let config = Config {
        socket: socket.clone(),
        name: "styx-native-run-test".to_string(),
        board: parse(text).expect("board"),
        drivers: vec![
            entry::<sim::SimSensor>(),
            entry::<sim::SimReceiver>(),
            entry::<sim::SimIsp>(),
        ],
        backend: Box::new(SimBoard::new()),
        options: Options {
            call_deadline: Duration::from_millis(200),
            ..Options::default()
        },
    };
    (Service::start(config).expect("start lemnosd"), socket)
}

/// Runs a session on `text` until it streams, sets the fault with `provoke`, and returns the
/// session's error and status once its stream has ended.
fn ended_by(name: &str, text: &str, provoke: impl FnOnce()) -> (Error, State, String) {
    let (service, socket) = start(name, text);
    let client = Client::connect(&socket, "styx-native-run-test").expect("connect");
    let found = styx_native::discover(&client).expect("discover");
    let camera = &found.cameras[0];
    let clock = SystemClock::default();
    let policy = Policy {
        frame_timeout: Duration::from_millis(500),
        ..Policy::default()
    };
    let mut session =
        Session::new(&client, camera, REQUEST, policy, &clock).with_controls(CONTROLS);
    session.start().expect("start");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut frames = 0;
    while frames < 4 {
        assert!(
            Instant::now() < deadline,
            "{name}: no frames before the fault"
        );
        if let Event::Frame(_) = session
            .next_event(Duration::from_millis(50))
            .expect("next_event before the fault")
        {
            frames += 1;
        }
    }

    provoke();
    let deadline = Instant::now() + Duration::from_secs(10);
    let error = loop {
        assert!(Instant::now() < deadline, "{name}: the stream did not end");
        match session.next_event(Duration::from_millis(50)) {
            Ok(_) => {}
            Err(error) => break error,
        }
    };
    let status = session.status().clone();
    drop(session);
    drop(service);
    (error, status.state, status.last_error.unwrap_or_default())
}

#[test]
fn a_receiver_turned_off_ends_as_device_failed_and_a_faulted_consumer_as_consumer_fault() {
    // 1. The receiver is gone and its board entry allows one attempt: the host turns it off for good
    //    and ends its stream, device failed.
    let board = sim::board_from(sim::Board {
        sensor_link: Some("csi0"),
        receiver_auto_off_after: Some(1),
        ..sim::Board::default()
    });
    let (error, state, last) = ended_by("off", &board, || {
        sim::GONE_RECEIVER.store(true, Ordering::SeqCst);
    });
    sim::GONE_RECEIVER.store(false, Ordering::SeqCst);
    match error {
        Error::StreamEnded { end, text } => {
            assert_eq!(end, StreamEnd::DeviceFailed);
            assert!(text.contains("the device producing it failed"), "{text}");
        }
        other => panic!("device failed: expected StreamEnded, got {other:?}"),
    }
    assert_eq!(state, State::Stopped);
    assert!(last.contains("the device producing it failed"), "{last}");

    // 2. The receiver ends its consumer for breaking the rules: a consumer fault, which says the
    //    reader (Styx) is at fault, not the camera.
    let (error, state, last) = ended_by("fault", &sim::board(Some("csi0")), || {
        sim::FAULT_CONSUMER.store(true, Ordering::SeqCst);
    });
    match error {
        Error::StreamEnded { end, text } => {
            assert_eq!(end, StreamEnd::ConsumerFault);
            assert!(text.contains("this reader broke the rules"), "{text}");
        }
        other => panic!("consumer fault: expected StreamEnded, got {other:?}"),
    }
    assert_eq!(state, State::Stopped);
    assert!(last.contains("this reader broke the rules"), "{last}");
}
