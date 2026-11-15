//! A camera that fails and comes back, on the simulated backend: the Lemnos stream reports the
//! stall once (the stream stays attached and quiet), then the return, and frames continue. Two
//! recoveries: the same instance (`Kept`: the sequence goes on) and a bind again (`Lost`).
use std::time::Duration;

use styx_lemnos::Carried;
use styx_sim::{Fault, SimCamera, SimEvent};

const WAIT: Duration = Duration::from_secs(10);
const REASON: &str = "bus error while streaming: restarting the sensor";

/// Waits for the next event that is not idle, up to [`WAIT`].
fn next_edge(cam: &mut SimCamera) -> SimEvent {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        match cam
            .next_event(Duration::from_millis(50))
            .expect("next event")
        {
            SimEvent::Idle => {
                assert!(std::time::Instant::now() < deadline, "no event in {WAIT:?}");
            }
            other => return other,
        }
    }
}

/// Reads frames until `count` have come, returning the last frame's identity.
fn frames(cam: &mut SimCamera, count: u32) -> u64 {
    let mut last = None;
    for _ in 0..count {
        match next_edge(cam) {
            SimEvent::Frame(frame) => {
                if let Some(prev) = last {
                    assert_eq!(frame.id(), prev + 1, "the sequence goes on");
                }
                last = Some(frame.id());
            }
            other => panic!("expected a frame, got {other:?}"),
        }
    }
    last.expect("a frame")
}

#[test]
fn a_transient_failure_is_one_stall_then_a_kept_return_and_the_sequence_goes_on() {
    let mut cam = SimCamera::start(styx_sim::Scene::Pattern, 0).expect("start the camera");
    let before = frames(&mut cam, 3);

    cam.inject_fault(Some(Fault::Failed(REASON)));
    match next_edge(&mut cam) {
        SimEvent::Stalled { since_ns } => assert!(since_ns > 0, "the failure has a time"),
        other => panic!("the stall comes first, got {other:?}"),
    }
    // Told once: the next waits are quiet while the sensor is down.
    for _ in 0..3 {
        assert!(matches!(
            cam.next_event(Duration::from_millis(30)).expect("wait"),
            SimEvent::Idle
        ));
    }

    cam.inject_fault(None);
    // Let the host read the sensor's health on its tick (a millisecond) before frames are granted
    // again: the return is then reported before the first frame, as the stream guarantees.
    std::thread::sleep(Duration::from_millis(100));
    match next_edge(&mut cam) {
        SimEvent::Resumed(Carried::Kept) => {}
        other => panic!("the same instance comes back kept, got {other:?}"),
    }
    // The first frame after the return continues the sequence: nothing was numbered while down.
    let after = match next_edge(&mut cam) {
        SimEvent::Frame(frame) => frame.id(),
        other => panic!("a frame after the return, got {other:?}"),
    };
    assert_eq!(after, before + 1, "a stall numbers no frame");
    frames(&mut cam, 2);
}

#[test]
fn a_device_bound_again_is_a_stall_then_a_lost_return() {
    let mut cam = SimCamera::start(styx_sim::Scene::Pattern, 0).expect("start the camera");
    frames(&mut cam, 2);

    cam.inject_fault(Some(Fault::Gone("the sensor's handle is dead")));
    assert!(matches!(next_edge(&mut cam), SimEvent::Stalled { .. }));
    // The device stays dead until the fault clears; the host binds it again on its backoff, and a
    // bind that finds it healthy is the return, which says the device was bound again.
    cam.inject_fault(None);
    // The host binds the device again on its backoff; frames are granted only by the next wait, so
    // the bind is finished before the wait that reads the return.
    std::thread::sleep(Duration::from_millis(600));
    match next_edge(&mut cam) {
        SimEvent::Resumed(Carried::Lost) => {}
        other => panic!("a rebound device comes back lost, got {other:?}"),
    }
}
