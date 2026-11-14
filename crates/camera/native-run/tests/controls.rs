//! Controls on a running camera: each delivered frame carries the metadata of its session's native
//! controls, and a value posted through the session lands on the frame it names.
//!
//! Runs on the simulated backend (the `styx-sim` drivers): `lemnosd` in this process on `lemnos-sim`, through
//! the real socket. The sensor's delays are the mode's: exposure two frames, gain one.

use styx_sim::board::{Board, Family};
use styx_sim::drivers::{SimIsp, SimReceiver, SimSensor};

use std::path::PathBuf;
use std::time::Duration;

use lemnos_contract_camera_receiver::BayerOrder;
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_controls::{Control, ControlValue, Update};
use styx_lemnos::Client;
use styx_native_run::{Error, Initial, Policy, Request, Session, State, SystemClock};

const START: Initial = Initial {
    exposure_us: 8_000,
    gain_milli: 1_000,
    frame_rate_milli: 60_000,
};

fn socket_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-native-run-controls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start(name: &str) -> (Service, PathBuf) {
    let dir = socket_dir().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("sock");
    let config = Config {
        socket: socket.clone(),
        name: "styx-native-run-test".to_string(),
        board: parse(&Board::timed(Family::Run).text()).expect("board"),
        drivers: vec![
            entry::<SimSensor>(),
            entry::<SimReceiver>(),
            entry::<SimIsp>(),
        ],
        backend: Box::new(SimBoard::new()),
        options: Options {
            call_deadline: Duration::from_millis(200),
            ..Options::default()
        },
    };
    (Service::start(config).expect("start lemnosd"), socket)
}

fn request() -> Request {
    Request {
        width: 320,
        height: 40,
        fps_milli: 60_000,
        bit_depth: Some(10),
        format: None,
        bayer: Some(BayerOrder::Rggb),
    }
}

#[test]
fn a_value_posted_to_a_running_camera_lands_on_its_frame_and_each_frame_carries_its_metadata() {
    let (service, socket) = start("land");
    let client = Client::connect(&socket, "styx-native-run-test").expect("connect");
    let found = styx_native::discover(&client).expect("discover");
    let camera = &found.cameras[0];
    let clock = SystemClock::default();
    let mut session =
        Session::new(&client, camera, request(), Policy::default(), &clock).with_controls(START);

    // Before the session streams there is no frame to land on.
    assert!(matches!(
        session.request_controls(&[]),
        Err(Error::NotStreaming)
    ));
    session.start().expect("start");
    assert_eq!(session.status().state, State::Streaming);

    // Exposure 8 ms until a step to 12 ms on frame TARGET. The step is posted during frame TARGET
    // minus the exposure delay (2), so it lands on TARGET. Frame ids come from the stream.
    const EXPOSURE_STEP_US: u32 = 12_000;
    let mut target = None;
    let mut seen = 0u64;
    let mut last_id = None;
    let mut stepped_on = None;
    while seen < 40 {
        let Some(lease) = session
            .next_frame(Duration::from_millis(500))
            .expect("next_frame")
        else {
            continue;
        };
        let id = lease.id();
        if let Some(last) = last_id {
            assert_eq!(id, last + 1, "frames arrive in order");
        }
        last_id = Some(id);
        if target.is_none() && id == 5 {
            let want = id + 6;
            let landings = session
                .request_controls(&[Update {
                    control: Control::Exposure,
                    value: ControlValue::Manual(EXPOSURE_STEP_US),
                    frame: want,
                }])
                .expect("request accepted");
            let landing = landings.iter().next().expect("one landing");
            assert_eq!(landing.frame, want, "predicted to land on its frame");
            assert!(!landing.late());
            target = Some(want);
        }
        let meta = lease.meta();
        let exposure = meta.exposure_us;
        let want_exposure = match target {
            Some(t) if id >= t => EXPOSURE_STEP_US,
            _ => START.exposure_us,
        };
        assert_eq!(
            exposure, want_exposure,
            "frame {id}: exposure in its metadata"
        );
        assert_eq!(
            meta.analogue_gain_milli, START.gain_milli,
            "frame {id}: gain in its metadata"
        );
        assert_eq!(
            meta.control(Control::Exposure),
            ControlValue::Manual(want_exposure),
            "frame {id}: the exposure control in force"
        );
        if stepped_on.is_none() && exposure == EXPOSURE_STEP_US {
            stepped_on = Some(id);
        }
        drop(lease);
        seen += 1;
    }
    let t = target.expect("requested");
    assert_eq!(stepped_on, Some(t), "the step shows on the frame it named");
    session.stop().expect("stop");
    drop(session);
    drop(client);
    service.stop();
}

#[test]
fn controls_must_be_configured() {
    let (service, socket) = start("refuse");
    let client = Client::connect(&socket, "styx-native-run-test").expect("connect");
    let found = styx_native::discover(&client).expect("discover");
    let camera = &found.cameras[0];
    let clock = SystemClock::default();

    let mut none = Session::new(&client, camera, request(), Policy::default(), &clock);
    assert_eq!(none.start(), Err(Error::NoControls));
    assert_eq!(none.status().state, State::Stopped);
    drop(none);
    drop(client);
    service.stop();
}
