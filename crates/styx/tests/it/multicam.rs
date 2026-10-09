//! A `FrameGrouper` over two reconnecting camera service clients: groups form, a service
//! restart is reported as `Disconnected` / `Connected`, partial groups carry on without the
//! missing camera meanwhile, and complete groups come back with it.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use styx::capture_api::make_virtual_device;
use styx::ipc::{CameraService, CameraServiceHandle, FrameClient};
use styx::multicam::{ClockMode, FrameGrouper, GroupConfig, GroupEvent, GroupPolicy};
use styx::prelude::*;

const MS: u64 = 1_000_000;

fn socket(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("styx-multicam-{name}-{}.sock", std::process::id()))
}

fn serve(path: &PathBuf, name: &str) -> CameraServiceHandle {
    let mode = Mode::with_interval(
        MediaFormat::new(
            FourCc::NV12,
            Resolution::new(160, 100).unwrap(),
            ColorSpace::Srgb,
        ),
        Interval::from_fps(60).unwrap(),
    );
    CameraService::new(make_virtual_device(name, [mode]))
        .keep_streaming()
        .serve(path)
        .unwrap()
}

/// What the test looks at in an event.
#[derive(Debug, PartialEq)]
enum Seen {
    Complete,
    Partial(Vec<usize>),
    Connected(usize),
    Disconnected(usize),
}

/// Events until `until` is seen (10 s at most).
fn until(grouper: &mut FrameGrouper, until: &Seen) -> Vec<Seen> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut all = Vec::new();
    while Instant::now() < deadline {
        let seen = match grouper.recv_event(Duration::from_millis(500)) {
            RecvOutcome::Data(GroupEvent::Group(g)) if g.complete => Seen::Complete,
            RecvOutcome::Data(GroupEvent::Group(g)) => Seen::Partial(g.cameras().collect()),
            RecvOutcome::Data(GroupEvent::Connected(c)) => Seen::Connected(c),
            RecvOutcome::Data(GroupEvent::Disconnected(c)) => Seen::Disconnected(c),
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => panic!("closed: {all:?}"),
        };
        let done = &seen == until;
        if all.last() != Some(&seen) {
            all.push(seen);
        }
        if done {
            return all;
        }
    }
    panic!("never saw {until:?}: {all:?}");
}

#[test]
fn reconnecting_clients_drop_out_of_groups_and_come_back() {
    let (left_path, right_path) = (socket("left"), socket("right"));
    let _left_service = serve(&left_path, "left");
    let mut right_service = Some(serve(&right_path, "right"));
    let client = |path: &PathBuf| {
        FrameClient::options(path)
            .reconnecting()
            .request(&Frames::nv12())
            .unwrap()
    };
    // Two free-running 60 fps virtual cameras: grouped by arrival, within half a frame; a
    // missing camera is not waited for more than 30 ms.
    let config = GroupConfig::new(8 * MS)
        .policy(GroupPolicy::Partial)
        .min_members(1)
        .deadline_ns(30 * MS);
    let mut grouper = FrameGrouper::new(config)
        .unwrap()
        .named("it-reconnect")
        .clock(ClockMode::Arrival);
    let left = grouper.add("left", client(&left_path)).unwrap();
    let right = grouper.add("right", client(&right_path)).unwrap();
    until(&mut grouper, &Seen::Complete);

    drop(right_service.take());
    let lost = until(&mut grouper, &Seen::Disconnected(right));
    assert!(!lost.contains(&Seen::Disconnected(left)), "{lost:?}");
    assert!(!grouper.is_connected(right));
    // Only the left camera now, without waiting for the right one.
    assert_eq!(
        until(&mut grouper, &Seen::Partial(vec![left])).last(),
        Some(&Seen::Partial(vec![left]))
    );

    right_service = Some(serve(&right_path, "right"));
    until(&mut grouper, &Seen::Connected(right));
    until(&mut grouper, &Seen::Complete);
    assert!(grouper.is_connected(right));

    let report = grouper.report();
    assert!(report.groups_complete >= 2, "{report:?}");
    assert!(report.groups_partial >= 1, "{report:?}");
    let spread = report.spread_max_ns.unwrap();
    assert!(spread <= 8 * MS, "{spread}");
    assert!(grouper.held() <= 2 * (3 + 2));
    drop(right_service);
}
