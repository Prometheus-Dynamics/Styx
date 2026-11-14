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
use styx_lemnos::{Carried, Client, StreamEnd};
use styx_native_run::{AutoOff, Clock, Error, Event, Initial, Policy, Request, Session, State};

/// The sensor's initial values: exposure 8 ms, gain 1.0x, 60 fps.
const CONTROLS: Initial = Initial {
    exposure_us: 8_000,
    gain_milli: 1_000,
    frame_rate_milli: 60_000,
};

/// A clock that records each backoff wait and returns at once. Its time is the real time that has
/// passed (so a silence measured while the test polls is real) plus the backoff waits, which
/// are simulated. A session can be told to cancel after a number of waits, as a stop from another
/// thread would.
struct FakeClock {
    origin: Instant,
    simulated: Cell<Duration>,
    waits: RefCell<Vec<Duration>>,
    cancel_after: Cell<Option<usize>>,
}

impl Default for FakeClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
            simulated: Cell::new(Duration::ZERO),
            waits: RefCell::new(Vec::new()),
            cancel_after: Cell::new(None),
        }
    }
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
        self.origin.elapsed() + self.simulated.get()
    }

    fn wait(&self, for_: Duration, cancel: &AtomicBool) {
        self.waits.borrow_mut().push(for_);
        self.simulated.set(self.simulated.get() + for_);
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

fn daemon_config(socket: &std::path::Path, text: &str) -> Config {
    Config {
        socket: socket.to_path_buf(),
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
    }
}

fn start(name: &str, text: &str) -> (Service, PathBuf) {
    let dir = socket_dir().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("sock");
    let service = Service::start(daemon_config(&socket, text)).expect("start lemnosd");
    (service, socket)
}

/// Polls `next_event` until `done` accepts an event. Returns that event and how many frames came
/// before it.
fn until<C: Clock>(
    session: &mut Session<'_, C>,
    what: &str,
    mut done: impl FnMut(&Event) -> bool,
) -> (Event, usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut frames = 0;
    loop {
        assert!(Instant::now() < deadline, "{what}");
        let event = session.next_event(Duration::from_millis(20)).expect("next_event");
        if matches!(event, Event::Frame(_)) {
            frames += 1;
        }
        if done(&event) {
            return (event, frames);
        }
    }
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
        fps_milli: 60_000,
        bit_depth: Some(10),
        format: None,
        bayer: Some(BayerOrder::Rggb),
    };
    let policy = Policy {
        frame_timeout: Duration::from_millis(500),
        stall_waits: 3,
        ..Policy::default()
    };

    // 1. The receiver fails its next eight starts. The waits double from 250 ms and hold at the
    //    5 s cap; the ninth start streams.
    let clock = FakeClock::default();
    let mut session =
        Session::new(&client, camera, request, policy, &clock).with_controls(CONTROLS);
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

    // 2. The receiver stops making frames and reports no fault: a silent camera. Nothing ends and
    //    nothing reopens. The no-frame report names the silence once its bound passes (one second
    //    at this rate), and the status says so. When frames come back, `Resumed(Kept)` comes before
    //    the first of them, on the same session.
    clock.clear();
    sim::STALL_NOW.store(true, Ordering::SeqCst);
    let (event, frames) = until(&mut session, "the silence report", |e| {
        matches!(e, Event::Stalled { .. })
    });
    match event {
        Event::Stalled { since_ns, text } => {
            assert_eq!(since_ns, 0, "a silence has no failure time");
            assert!(text.starts_with("no frame for "), "{text}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(frames, 0, "a silent camera makes no frame");
    assert_eq!(session.status().state, State::Streaming);
    assert_eq!(session.status().starts, 1);
    assert_eq!(session.status().total_failures, 8);
    assert!(
        session
            .status()
            .stall
            .as_deref()
            .is_some_and(|s| s.starts_with("no frame for ")),
        "{:?}",
        session.status().stall
    );
    assert!(clock.waits().is_empty(), "a silence waits no backoff");
    sim::STALL_NOW.store(false, Ordering::SeqCst);
    let (event, frames) = until(&mut session, "the return", |e| {
        matches!(e, Event::Resumed(_))
    });
    assert_eq!(frames, 0, "Resumed comes before the first frame after the silence");
    assert!(matches!(event, Event::Resumed(Carried::Kept)), "{event:?}");
    assert_eq!(session.status().state, State::Streaming);
    assert_eq!(session.status().starts, 1, "no reopen");
    assert_eq!(session.status().stall, None);
    assert!(clock.waits().is_empty(), "no backoff after the return");
    frames_flow(&mut session, 4, "after the silence");
    drop(session);

    // 3. Auto-off as a policy: three failures in a row turn the session off, without a wait for
    //    the third. Starting it again re-arms it, and two more failures then a success stream.
    let clock = FakeClock::default();
    let policy = Policy {
        auto_off: AutoOff::AfterFailures(3.try_into().unwrap()),
        ..policy
    };
    let mut session =
        Session::new(&client, camera, request, policy, &clock).with_controls(CONTROLS);
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
    let mut session =
        Session::new(&client, camera, request, Policy::default(), &clock).with_controls(CONTROLS);
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

    // 5. A real end: the daemon stops, and the host ends the stream (its ring says closed). The
    //    session returns `StreamEnded` with the kind, is stopped and holds nothing; the owner
    //    reopens it. The reopen against a daemon that is not up yet backs off as a failed start does,
    //    and streams once the daemon answers.
    let clock = FakeClock::default();
    let mut session =
        Session::new(&client, camera, request, Policy::default(), &clock).with_controls(CONTROLS);
    session.start().expect("start before the end");
    frames_flow(&mut session, 2, "before the end");
    service.stop();
    // A bound on the owner's waits: the stop must be the end, not a retry loop with no daemon.
    clock.cancel_after.set(Some(64));
    let deadline = Instant::now() + Duration::from_secs(10);
    let end = loop {
        assert!(Instant::now() < deadline, "no end after the daemon stopped");
        match session.next_event(Duration::from_millis(20)) {
            Ok(_) => {}
            Err(e) => break e,
        }
    };
    match end {
        Error::StreamEnded { end, .. } => assert_eq!(end, StreamEnd::Closed),
        other => panic!("expected the stream's end, got {other:?}"),
    }
    assert_eq!(session.status().state, State::Stopped);
    assert!(session.selection().is_none(), "the end leaves nothing running");
    drop(session);
    drop(client);

    let service = Service::start(daemon_config(&socket, &sim::board(Some("csi0"))))
        .expect("the daemon starts again");
    let client = Client::connect(&socket, "styx-native-run-test").expect("connect again");
    let found = styx_native::discover(&client).expect("discover again");
    let camera = &found.cameras[0];
    let clock = FakeClock::default();
    let mut session =
        Session::new(&client, camera, request, Policy::default(), &clock).with_controls(CONTROLS);
    sim::FAIL_STARTS.store(2, Ordering::SeqCst);
    session.start().expect("the reopen streams after two refused starts");
    assert_eq!(clock.waits(), ms(&[250, 500]));
    frames_flow(&mut session, 2, "after the reopen");
    session.stop().expect("stop");
    drop(session);
    drop(client);
    service.stop();
}
