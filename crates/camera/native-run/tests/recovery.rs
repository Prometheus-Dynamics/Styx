//! A forced receiver fault recovers with capped backoff, on the simulated backend.
//!
//! The clock is a fake: each backoff is recorded and returns at once, so the test checks the
//! delays the session chose without waiting for them. Every fault switch is in this one test,
//! because the switches are process-wide and the test binary runs its tests in parallel.

mod sim;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lemnos_contract_camera_receiver::BayerOrder;
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_lemnos::Client;
use styx_native_run::{AutoOff, Clock, Error, Policy, Request, Session, State};

/// A clock that records each wait and returns at once. A session can be told to cancel after a
/// number of waits, as a stop from another thread would.
#[derive(Default)]
struct FakeClock {
    now: Cell<Duration>,
    waits: RefCell<Vec<Duration>>,
    cancel_after: Cell<Option<usize>>,
}

impl FakeClock {
    fn waits(&self) -> Vec<Duration> {
        self.waits.borrow().clone()
    }

    fn clear(&self) {
        self.waits.borrow_mut().clear();
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        self.now.get()
    }

    fn wait(&self, for_: Duration, cancel: &AtomicBool) {
        self.waits.borrow_mut().push(for_);
        self.now.set(self.now.get() + for_);
        if self.cancel_after.get() == Some(self.waits.borrow().len()) {
            cancel.store(true, Ordering::Release);
        }
    }
}

fn socket_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-native-run-recover-{}", std::process::id()));
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

fn ms(values: &[u64]) -> Vec<Duration> {
    values.iter().map(|v| Duration::from_millis(*v)).collect()
}

/// Calls `next_frame` until `want` frames arrive, with a deadline.
fn frames_flow<C: Clock>(session: &mut Session<'_, C>, want: usize, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = 0;
    while seen < want {
        assert!(
            Instant::now() < deadline,
            "{what}: frames stopped at {seen}"
        );
        if session
            .next_frame(Duration::from_millis(100))
            .expect("next_frame")
            .is_some()
        {
            seen += 1;
        }
    }
}

#[test]
fn a_receiver_fault_recovers_with_capped_backoff() {
    let (service, socket) = start("recover", &sim::board(Some("csi0")));
    let client = Client::connect(&socket, "styx-native-run-test").expect("connect");
    let found = styx_native::discover(&client).expect("discover");
    assert!(found.refused.is_empty(), "refused: {:?}", found.refused);
    let camera = &found.cameras[0];
    let request = Request {
        width: 320,
        height: 40,
        min_fps_milli: 60_000,
        bit_depth: Some(10),
        bayer: Some(BayerOrder::Rggb),
    };
    // The frame-wait limits the test uses: a stall is three 20 ms waits with no frame.
    let policy = Policy {
        frame_timeout: Duration::from_millis(500),
        stall_waits: 3,
        ..Policy::default()
    };

    // 1. The receiver fails its next eight starts. The waits double from 250 ms and hold at the
    //    5 s cap; the ninth start streams.
    let clock = FakeClock::default();
    let mut session = Session::new(&client, camera, request, policy, &clock);
    sim::FAIL_STARTS.store(8, Ordering::SeqCst);
    session
        .start()
        .expect("comes back after eight failed starts");
    assert_eq!(
        clock.waits(),
        ms(&[250, 500, 1000, 2000, 4000, 5000, 5000, 5000])
    );
    assert_eq!(sim::FAIL_STARTS.load(Ordering::SeqCst), 0);
    let status = session.status().clone();
    assert_eq!(status.state, State::Streaming);
    assert_eq!(status.starts, 1);
    assert_eq!(status.total_failures, 8);
    assert_eq!(status.consecutive_failures, 0);
    assert!(
        status
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("receiver.start refused")),
        "last error: {:?}",
        status.last_error
    );
    frames_flow(&mut session, 4, "after the failed starts");

    // 2. The receiver stops making frames mid-stream. Three empty 20 ms waits make a stall: the
    //    devices stop (which clears the fault, as a receiver reset does), one backoff runs, and the
    //    camera starts again inside next_frame.
    clock.clear();
    sim::STALL_NOW.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(10);
    while session.status().starts == 1 {
        assert!(Instant::now() < deadline, "no recovery from the stall");
        let _ = session
            .next_frame(Duration::from_millis(20))
            .expect("next_frame");
    }
    assert!(!sim::STALL_NOW.load(Ordering::SeqCst));
    assert_eq!(clock.waits(), ms(&[250]));
    assert_eq!(session.status().state, State::Streaming);
    assert_eq!(session.status().starts, 2);
    assert_eq!(session.status().total_failures, 9);
    frames_flow(&mut session, 4, "after the stall");
    drop(session);

    // 3. Auto-off as a policy: three failures in a row turn the session off, without a wait for
    //    the third. Starting it again re-arms it, and two more failures then a success stream.
    let clock = FakeClock::default();
    let policy = Policy {
        auto_off: AutoOff::AfterFailures(3.try_into().unwrap()),
        ..policy
    };
    let mut session = Session::new(&client, camera, request, policy, &clock);
    sim::FAIL_STARTS.store(5, Ordering::SeqCst);
    match session.start() {
        Err(Error::AutoOff { failures, .. }) => assert_eq!(failures, 3),
        other => panic!("expected auto-off, got {other:?}"),
    }
    assert_eq!(clock.waits(), ms(&[250, 500]));
    assert_eq!(session.status().state, State::Off);
    assert_eq!(sim::FAIL_STARTS.load(Ordering::SeqCst), 2);
    clock.clear();
    session.start().expect("started again after auto-off");
    assert_eq!(clock.waits(), ms(&[250, 500]));
    assert_eq!(session.status().state, State::Streaming);
    frames_flow(&mut session, 2, "after a restart");
    session.stop().expect("stop");
    drop(session);

    // 4. A manual stop during a backoff ends the wait at once; start then returns Stopped.
    //    Starting again, with the fault cleared, streams.
    let clock = FakeClock::default();
    clock.cancel_after.set(Some(3));
    let mut session = Session::new(&client, camera, request, Policy::default(), &clock);
    sim::FAIL_STARTS.store(100, Ordering::SeqCst);
    match session.start() {
        Err(Error::Stopped) => {}
        other => panic!("expected a stop, got {other:?}"),
    }
    assert_eq!(clock.waits(), ms(&[250, 500, 1000]));
    assert_eq!(session.status().state, State::Stopped);
    sim::FAIL_STARTS.store(0, Ordering::SeqCst);
    session.start().expect("starts once the fault is gone");
    frames_flow(&mut session, 2, "after the manual restart");
    session.stop().expect("stop");

    drop(session);
    drop(client);
    service.stop();
}
