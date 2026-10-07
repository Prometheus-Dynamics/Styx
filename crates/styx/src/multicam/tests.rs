//! Synthetic cameras (frames with chosen timestamps and clocks, through queues) into a
//! `FrameGrouper`: the sync, polled and async APIs, clocks, sources closing, leases returning.

use std::os::fd::AsRawFd;
use std::thread;
use std::time::{Duration, Instant};

use super::*;

const MS: u64 = 1_000_000;
const PERIOD: u64 = 33_333_333;

fn frame(pool: &BufferPool, timestamp: u64, clock: Option<TimestampClock>) -> FrameLease {
    let res = Resolution::new(8, 8).unwrap();
    let mut meta = FrameMeta::new(
        MediaFormat::new(FourCc::GREY, res, ColorSpace::Srgb),
        timestamp,
    );
    meta.clock = clock;
    FrameLease::single_plane(meta, pool.lease(), 64, 8)
}

fn readable(fd: i32, ms: i32) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: a valid pollfd for the call's duration.
    unsafe { libc::poll(&mut p, 1, ms) };
    p.revents != 0
}

fn monotonic_now() -> u64 {
    TimestampClock::Monotonic.now_ns().unwrap()
}

struct Cam {
    tx: BoundedTx<FrameLease>,
    pool: BufferPool,
}

fn cameras(grouper: &mut FrameGrouper, n: usize) -> Vec<Cam> {
    (0..n)
        .map(|i| {
            let (tx, rx) = bounded(8);
            grouper.add(format!("cam{i}"), rx).unwrap();
            Cam {
                tx,
                pool: BufferPool::with_capacity(4, 64),
            }
        })
        .collect()
}

#[test]
fn queued_cameras_group_and_every_lease_returns() {
    let config = GroupConfig::new(PERIOD / 2).depth(2).output_depth(1);
    let mut grouper = FrameGrouper::new(config).unwrap().named("unit-queued");
    let cams = cameras(&mut grouper, 3);
    let base = monotonic_now();
    let mut groups = 0;
    for n in 0..200u64 {
        for (i, cam) in cams.iter().enumerate() {
            let jitter = (n * 7919 + i as u64 * 104_729) % 600_000;
            let ts = base + n * PERIOD + i as u64 * MS + jitter;
            cam.tx
                .send(frame(&cam.pool, ts, Some(TimestampClock::Monotonic)));
        }
        // A consumer that looks only every third turn: older groups are replaced (and their
        // frames released), never piled up.
        if n.is_multiple_of(3) {
            while let RecvOutcome::Data(group) = grouper.try_next() {
                assert!(group.complete);
                assert_eq!(group.len(), 3);
                assert!(group.spread_ns < 3 * MS, "{}", group.spread_ns);
                groups += 1;
            }
        }
        let held: usize = cams.iter().map(|c| c.pool.stats().in_use).sum();
        // Frames pending in the grouper, a group ready, and up to two turns in the queues.
        assert!(held <= 3 * (2 + 1) + 3 * 2, "{held} leases out");
        assert!(grouper.held() <= 3 * (2 + 1));
    }
    assert!(groups >= 60, "{groups}");
    let report = grouper.report();
    // Two turns of frames wait in the queues at times: a camera's oldest overflows.
    assert!(report.groups_complete >= 130, "{report:?}");
    assert!(report.drops_of(DropReason::Stale) > 0);
    assert_eq!(report.drops_of(DropReason::Unmatched), 0);
    let offset = report.cameras[2].offset_mean_ns.unwrap();
    assert!((1.5e6..2.5e6).contains(&offset), "{offset}");

    // The process snapshot lists it, and Prometheus shows it.
    let snapshot = crate::metrics::snapshot();
    let mine = snapshot
        .sync_groups
        .iter()
        .find(|g| g.name == "unit-queued")
        .expect("listed");
    assert_eq!(mine.cameras, ["cam0", "cam1", "cam2"]);
    assert_eq!(mine.policy, "strict");
    let text = snapshot.prometheus_text();
    for needle in [
        "styx_sync_groups_total{group=\"unit-queued\",kind=\"complete\"}",
        "styx_sync_spread_ms{group=\"unit-queued\",quantile=\"0.99\"}",
        "styx_sync_drops_total{group=\"unit-queued\",camera=\"cam1\",reason=\"stale\"}",
        "styx_sync_drift_ppm{group=\"unit-queued\",camera=\"cam2\"}",
        "styx_sync_match_ratio{group=\"unit-queued\"}",
    ] {
        assert!(text.contains(needle), "{needle} missing");
    }

    // Take the last frames out of the queues, then let everything go.
    while let RecvOutcome::Data(_) = grouper.try_next() {}
    grouper.clear();
    assert_eq!(grouper.held(), 0);
    drop(grouper);
    for cam in &cams {
        assert_eq!(cam.pool.stats().in_use, 0, "a lease was kept");
    }
    assert!(
        !crate::metrics::snapshot()
            .sync_groups
            .iter()
            .any(|g| g.name == "unit-queued")
    );
}

#[test]
fn boottime_and_monotonic_cameras_share_a_clock() {
    let mut grouper = FrameGrouper::new(GroupConfig::new(2 * MS)).unwrap();
    let cams = cameras(&mut grouper, 2);
    for _ in 0..5 {
        // The same instant on both clocks (they differ by the time spent suspended).
        let mono = monotonic_now();
        let boot = TimestampClock::Boottime.now_ns().unwrap();
        cams[0]
            .tx
            .send(frame(&cams[0].pool, mono, Some(TimestampClock::Monotonic)));
        cams[1]
            .tx
            .send(frame(&cams[1].pool, boot, Some(TimestampClock::Boottime)));
        let RecvOutcome::Data(group) = grouper.recv(Duration::from_secs(1)) else {
            panic!("no group");
        };
        assert!(group.spread_ns < MS / 2, "{}", group.spread_ns);
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn frames_on_unusable_clocks_are_refused_clearly() {
    let mut grouper = FrameGrouper::new(GroupConfig::new(MS)).unwrap();
    let cams = cameras(&mut grouper, 2);
    cams[0].tx.send(frame(&cams[0].pool, 1, None));
    assert!(matches!(grouper.try_next(), RecvOutcome::Empty));
    assert!(matches!(
        grouper.last_error(),
        Some(ClockError::Unknown { camera }) if camera == "cam0"
    ));
    cams[1].tx.send(frame(
        &cams[1].pool,
        1,
        Some(TimestampClock::StreamRelative),
    ));
    assert!(matches!(grouper.try_next(), RecvOutcome::Empty));
    let err = grouper.last_error().unwrap();
    assert!(matches!(err, ClockError::Unrelated { .. }));
    assert!(err.to_string().contains("ClockMode::Raw"), "{err}");
    assert_eq!(grouper.report().drops_of(DropReason::Clock), 2);
    assert_eq!(cams[0].pool.stats().in_use + cams[1].pool.stats().in_use, 0);

    // Raw: one timeline, whatever it is, but the same for all.
    let mut raw = FrameGrouper::new(GroupConfig::new(MS))
        .unwrap()
        .clock(ClockMode::Raw);
    let cams = cameras(&mut raw, 2);
    let rel = Some(TimestampClock::StreamRelative);
    cams[0].tx.send(frame(&cams[0].pool, 10 * MS, rel));
    cams[1].tx.send(frame(&cams[1].pool, 10 * MS + 10, rel));
    assert!(matches!(raw.try_next(), RecvOutcome::Data(g) if g.spread_ns == 10));
    cams[1].tx.send(frame(
        &cams[1].pool,
        20 * MS,
        Some(TimestampClock::Monotonic),
    ));
    assert!(matches!(raw.try_next(), RecvOutcome::Empty));
    assert!(matches!(
        raw.last_error(),
        Some(ClockError::Mismatch { .. })
    ));
}

#[test]
fn arrival_groups_sources_without_a_sensor_clock() {
    let mut grouper = FrameGrouper::new(GroupConfig::new(5 * MS))
        .unwrap()
        .clock(ClockMode::Arrival);
    let cams = cameras(&mut grouper, 2);
    for cam in &cams {
        let mut f = frame(&cam.pool, 3, Some(TimestampClock::StreamRelative));
        f.meta_mut().hops.set(Hop::Dequeued, monotonic_now());
        cam.tx.send(f);
    }
    let RecvOutcome::Data(group) = grouper.recv(Duration::from_secs(1)) else {
        panic!("no group");
    };
    assert!(group.spread_ns < MS);
}

#[test]
fn the_descriptor_wakes_for_frames_and_for_deadlines() {
    let config = GroupConfig::new(PERIOD / 2)
        .policy(GroupPolicy::Partial)
        .deadline_ns(40 * MS)
        .min_members(1);
    let mut grouper = FrameGrouper::new(config).unwrap();
    let cams = cameras(&mut grouper, 2);
    let fd = grouper.as_raw_fd();
    // Adding cameras made it readable once: drain.
    assert!(matches!(grouper.try_next(), RecvOutcome::Empty));
    assert!(!readable(fd, 50), "readable with nothing to do");

    let (tx, pool) = (cams[0].tx.clone(), cams[0].pool.clone());
    let sender = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        tx.send(frame(
            &pool,
            monotonic_now(),
            Some(TimestampClock::Monotonic),
        ));
    });
    assert!(readable(fd, 2000), "a frame did not wake it");
    sender.join().unwrap();
    // Camera 1 has nothing: the group waits for its deadline.
    assert!(matches!(grouper.try_next(), RecvOutcome::Empty));
    let started = Instant::now();
    assert!(readable(fd, 2000), "the deadline did not wake it");
    assert!(started.elapsed() >= Duration::from_millis(20));
    let RecvOutcome::Data(group) = grouper.try_next() else {
        panic!("no partial group at the deadline");
    };
    assert!(!group.complete);
    assert_eq!(group.cameras().collect::<Vec<_>>(), [0]);
}

#[test]
fn groups_are_awaited_and_streamed() {
    let mut grouper = FrameGrouper::new(GroupConfig::new(PERIOD / 2)).unwrap();
    let cams = cameras(&mut grouper, 2);
    let senders: Vec<_> = cams
        .iter()
        .enumerate()
        .map(|(i, cam)| {
            let (tx, pool) = (cam.tx.clone(), cam.pool.clone());
            thread::spawn(move || {
                let base = monotonic_now();
                for n in 0..6u64 {
                    thread::sleep(Duration::from_millis(5));
                    let ts = base + n * PERIOD + i as u64 * MS;
                    tx.send(frame(&pool, ts, Some(TimestampClock::Monotonic)));
                }
            })
        })
        .collect();
    let RecvOutcome::Data(first) = styx_graph::rt::block_on(grouper.next()) else {
        panic!("no group");
    };
    assert_eq!(first.len(), 2);
    drop(first);
    for sender in senders {
        sender.join().unwrap();
    }
    for cam in &cams {
        cam.tx.close();
    }
    // The queues closed: the stream ends after the groups left.
    let mut stream = grouper.stream();
    let mut rest = 0;
    while let Some(group) = styx_graph::rt::block_on(styx_graph::rt::next(&mut stream)) {
        assert_eq!(group.len(), 2);
        rest += 1;
    }
    assert!(rest >= 1, "{rest}");
}

#[test]
fn closing_sources_report_disconnects_then_close() {
    let config = GroupConfig::new(MS).policy(GroupPolicy::Partial);
    let mut grouper = FrameGrouper::new(config).unwrap();
    let cams = cameras(&mut grouper, 2);
    cams[0].tx.close();
    let seen = |g: &mut FrameGrouper| match g.try_event() {
        RecvOutcome::Data(GroupEvent::Disconnected(c)) => Some(c),
        RecvOutcome::Data(other) => panic!("{other:?}"),
        _ => None,
    };
    assert_eq!(seen(&mut grouper), Some(0));
    assert!(!grouper.is_connected(0));
    assert!(matches!(grouper.try_next(), RecvOutcome::Empty));
    cams[1].tx.close();
    assert_eq!(seen(&mut grouper), Some(1));
    assert!(matches!(grouper.try_next(), RecvOutcome::Closed));
    assert!(grouper.remove(1).is_some());
    assert_eq!(grouper.camera_name(0), Some("cam0"));
    assert_eq!(grouper.camera_name(1), None);
}
