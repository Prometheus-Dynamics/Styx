//! A native session on the simulated camera. The session runs the chain through the socket
//! (sensor, receiver, frame stream) as the service does; the simulator's frame clock makes the
//! frames, and its faults are injected as styx-sim's stall tests inject them.

use std::time::{Duration, Instant};

use lemnos_contract_camera_receiver::BayerOrder;
use styx_controls::{Control, ControlValue, Update};
use styx_lemnos::{Carried, Client};
use styx_native_run::{Clock, Event, Initial, Policy, Request, Session, SystemClock};
use styx_sim::{Fault, SimRig};

/// The sensor's reason for its failure: the device's own text, as the status shows it.
const REASON: &str = "bus error while streaming: restarting the sensor";
const WAIT: Duration = Duration::from_secs(10);

const CONTROLS: Initial = Initial {
    exposure_us: 8_000,
    gain_milli: 1_000,
    frame_rate_milli: 60_000,
};

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

/// Polls the session until `done` accepts an event, with a deadline. Returns that event and the
/// ids of the frames met before it.
fn until<C: Clock>(
    session: &mut Session<'_, C>,
    what: &str,
    mut done: impl FnMut(&Event) -> bool,
) -> (Event, Vec<u64>) {
    let deadline = Instant::now() + WAIT;
    let mut ids = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "{what}");
        let event = session
            .next_event(Duration::from_millis(20))
            .expect("next_event");
        if let Event::Frame(lease) = &event {
            ids.push(lease.id());
        }
        if done(&event) {
            return (event, ids);
        }
    }
}

/// Reads `count` frames and returns the last one's id. Every frame's id goes on from the last.
fn frames<C: Clock>(session: &mut Session<'_, C>, count: u32) -> u64 {
    let mut last = None;
    let mut seen = 0;
    while seen < count {
        let (_, ids) = until(session, "frames", |e| matches!(e, Event::Frame(_)));
        for id in ids {
            if let Some(prev) = last {
                assert!(id > prev, "frame {id} after {prev}");
            }
            last = Some(id);
            seen += 1;
        }
    }
    last.expect("a frame")
}

/// The simulated camera's daemon, the client to it, and its camera, found as a session takes it.
fn camera(rig: &SimRig) -> (Client, styx_native::Camera) {
    let client = Client::connect(rig.socket(), "session-sim").expect("connect");
    let found = styx_native::discover(&client).expect("discover");
    assert!(found.refused.is_empty(), "refused: {:?}", found.refused);
    let camera = found.cameras.into_iter().next().expect("a camera");
    (client, camera)
}

#[test]
fn a_transient_failure_is_one_stall_then_a_kept_return_and_the_sequence_goes_on() {
    let rig = SimRig::start().expect("the simulated camera");
    let (client, camera) = camera(&rig);
    let clock = SystemClock::default();
    let mut session = Session::new(&client, &camera, request(), Policy::default(), &clock)
        .with_controls(CONTROLS);
    session.start().expect("start");
    let mut last = frames(&mut session, 3);

    // The sensor fails: the stream stays attached and one stall is reported, with its time and the
    // device's reason. Frames stop while it lasts.
    rig.inject_fault(Some(Fault::Failed(REASON)));
    let (event, ids) = until(&mut session, "the stall", |e| {
        matches!(e, Event::Stalled { .. })
    });
    if let Some(&id) = ids.last() {
        last = id;
    }
    match event {
        Event::Stalled { since_ns, text } => {
            assert!(since_ns > 0, "the failure has a time");
            assert!(
                text.starts_with("camera failed, retrying since ") && text.ends_with(REASON),
                "{text}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(
        session
            .status()
            .stall
            .as_deref()
            .is_some_and(|s| s.ends_with(REASON)),
        "{:?}",
        session.status().stall
    );
    for _ in 0..3 {
        let event = session.next_event(Duration::from_millis(30)).expect("wait");
        assert!(matches!(event, Event::Idle), "{event:?}");
    }

    // The reader's skips from here on are the only gap the sequence can show: none is made in the
    // stall, which numbers no frame.
    let skipped_at_stall = session.skipped();

    // The sensor is back: the same stream resumes, Kept, before the first frame after the stall.
    rig.inject_fault(None);
    let (event, ids) = until(&mut session, "the return", |e| {
        matches!(e, Event::Resumed(_))
    });
    assert!(
        ids.is_empty(),
        "Resumed comes before the first frame after the stall"
    );
    assert!(matches!(event, Event::Resumed(Carried::Kept)), "{event:?}");
    assert_eq!(session.status().stall, None);

    // The sequence goes on: the first frame after the return is the next number, plus any frames
    // the reader was too late for (which the session counts).
    let next = frames(&mut session, 1);
    let late = session.skipped() - skipped_at_stall;
    assert_eq!(next, last + 1 + late, "a stall numbers no frame");
    // No restart was counted: the same session streams on.
    assert_eq!(session.status().starts, 1);
    assert_eq!(session.status().total_failures, 0);
    frames(&mut session, 3);
}

#[test]
fn a_device_bound_again_resumes_lost_on_the_same_mode_and_posts_what_was_asked_again() {
    let rig = SimRig::start().expect("the simulated camera");
    let (client, camera) = camera(&rig);
    let clock = SystemClock::default();
    let mut session = Session::new(&client, &camera, request(), Policy::default(), &clock)
        .with_controls(CONTROLS);
    session.start().expect("start");
    let last = frames(&mut session, 3);
    let mode = session.status().mode;

    // Asked for, from a few frames on: exposure, gain and frame rate.
    let at = last + 4;
    session
        .request_controls(&[
            Update {
                control: Control::Exposure,
                value: ControlValue::Manual(4_000),
                frame: at,
            },
            Update {
                control: Control::Gain,
                value: ControlValue::Manual(2_000),
                frame: at,
            },
            Update {
                control: Control::FrameRate,
                value: ControlValue::Manual(50_000),
                frame: at,
            },
        ])
        .expect("controls");
    frames(&mut session, 6);
    let exposure = rig.written(Control::Exposure).expect("exposure written");
    let gain = rig.written(Control::Gain).expect("gain written");
    let writes = rig.writes();

    // The device is dead, then bound again: the session stalls, then the return says it was bound
    // again. The same mode is selected and the controls asked for are posted again.
    rig.inject_fault(Some(Fault::Gone("the sensor's handle is dead")));
    let (event, _) = until(&mut session, "the stall", |e| {
        matches!(e, Event::Stalled { .. })
    });
    assert!(matches!(event, Event::Stalled { .. }), "{event:?}");
    rig.inject_fault(None);
    let (event, ids) = until(&mut session, "the return", |e| {
        matches!(e, Event::Resumed(_))
    });
    assert!(
        ids.is_empty(),
        "Resumed comes before the first frame after the rebind"
    );
    assert!(matches!(event, Event::Resumed(Carried::Lost)), "{event:?}");

    assert_eq!(session.status().mode, mode, "the same mode");
    assert_eq!(session.status().starts, 1, "no reopen: the same stream");
    assert_eq!(rig.written(Control::Exposure), Some(exposure));
    assert_eq!(rig.written(Control::Gain), Some(gain));
    assert!(
        rig.writes() >= writes + 3,
        "the sensor took the exposure, gain and frame length again: {} -> {}",
        writes,
        rig.writes()
    );
    frames(&mut session, 4);
    assert_eq!(session.status().stall, None);
}
