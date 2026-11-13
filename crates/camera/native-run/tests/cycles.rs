//! Start and stop 100 times on the simulated backend.
//!
//! `lemnosd` runs in this process with the simulated sensor, receiver and ISP (see `sim/mod.rs`) on
//! `lemnos-sim`. Each cycle goes through the real socket: it selects the mode, configures the
//! receiver with the mode's link, starts receiver then sensor, drops the mode's first frames,
//! streams, and stops sensor then receiver.

mod sim;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use lemnos_contract_camera_receiver::BayerOrder;
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_lemnos::Client;
use styx_native_run::{GainLaw, Initial, Policy, Request, Session, State, SystemClock};

/// The sensor's initial values: exposure 8 ms, gain 1.0x, 60 fps.
const CONTROLS: Initial = Initial {
    exposure_us: 8_000,
    gain_milli: 1_000,
    frame_rate_milli: 60_000,
};

fn socket_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-native-run-sim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start(name: &str, text: &str) -> (Service, PathBuf) {
    let dir = socket_dir().join(name);
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

#[test]
fn start_and_stop_100_times_on_the_simulated_backend() {
    let (service, socket) = start("cycles", &sim::board(Some("csi0")));
    let client = Client::connect(&socket, "styx-native-run-test").expect("connect");
    let found = styx_native::discover(&client).expect("discover");
    assert!(found.refused.is_empty(), "refused: {:?}", found.refused);
    assert_eq!(found.cameras.len(), 1);
    let camera = &found.cameras[0];

    let request = Request {
        width: 320,
        height: 40,
        min_fps_milli: 60_000,
        bit_depth: Some(10),
        bayer: Some(BayerOrder::Rggb),
    };
    let clock = SystemClock::default();
    let mut session = Session::new(&client, camera, request, Policy::default(), &clock)
        .with_controls(CONTROLS, GainLaw::Linear);
    let order = [
        "sensor.select_mode",
        "receiver.configure",
        "sensor.set_controls",
        "receiver.start",
        "sensor.start",
        "sensor.stop",
        "receiver.stop",
    ];

    for cycle in 0..100 {
        session
            .start()
            .unwrap_or_else(|e| panic!("cycle {cycle}: start: {e}"));
        assert_eq!(session.status().state, State::Streaming, "cycle {cycle}");
        assert_eq!(session.status().mode, Some(0), "cycle {cycle}");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = 0;
        while seen < 4 {
            assert!(
                Instant::now() < deadline,
                "cycle {cycle}: frames stopped at {seen}"
            );
            if let Some(lease) = session
                .next_frame(Duration::from_millis(200))
                .expect("next_frame")
            {
                assert!(lease.timestamp_ns() > 0);
                seen += 1;
            }
        }

        session.stop().expect("stop");
        assert_eq!(session.status().state, State::Stopped, "cycle {cycle}");
        assert_eq!(sim::take_log(), order, "cycle {cycle}: call order");
    }

    let status = session.status();
    assert_eq!(status.starts, 100);
    assert_eq!(status.total_failures, 0);
    for counter in styx_frame::copy_counters() {
        assert_eq!(counter.copies(), 0, "{} copied", counter.site());
    }
    drop(session);
    drop(client);
    service.stop();
}
