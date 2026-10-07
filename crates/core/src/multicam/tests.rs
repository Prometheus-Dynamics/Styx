//! Synthetic cameras with controlled timestamps, jitter and drift.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::Cell;

use super::*;

const MS: u64 = 1_000_000;
/// 30 fps.
const PERIOD: u64 = 33_333_333;

/// A frame that counts itself while alive, like a capture buffer lease.
struct Lease {
    live: Rc<Cell<usize>>,
    tag: u64,
}

impl Lease {
    fn new(live: &Rc<Cell<usize>>, tag: u64) -> Self {
        live.set(live.get() + 1);
        Self {
            live: live.clone(),
            tag,
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.live.set(self.live.get() - 1);
    }
}

/// Deterministic jitter: xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in [-amplitude, amplitude].
    fn jitter(&mut self, amplitude: u64) -> i64 {
        if amplitude == 0 {
            return 0;
        }
        (self.next() % (2 * amplitude + 1)) as i64 - amplitude as i64
    }
}

/// A free-running camera: `offset` from the epoch, `period`, `drift_ppm` on its clock, `jitter`
/// on each timestamp.
struct SynthCam {
    offset: u64,
    period: u64,
    drift_ppm: f64,
    jitter: u64,
    n: u64,
}

impl SynthCam {
    fn new(offset: u64, period: u64) -> Self {
        Self {
            offset,
            period,
            drift_ppm: 0.0,
            jitter: 0,
            n: 0,
        }
    }

    fn next_ts(&mut self, rng: &mut Rng) -> u64 {
        let ideal = self.offset + self.n * self.period;
        self.n += 1;
        let drifted = ideal as f64 * (1.0 + self.drift_ppm * 1e-6);
        (drifted as i64 + rng.jitter(self.jitter)) as u64
    }
}

/// Runs `cams` in timestamp order (frames arrive 5 ms after their timestamp), polling after
/// every frame and collecting what pops out.
fn run(
    grouper: &mut Grouper<Lease>,
    cams: &mut [SynthCam],
    until: u64,
    live: &Rc<Cell<usize>>,
    skip: impl Fn(usize, u64) -> bool,
) -> Vec<FrameGroup<Lease>> {
    let mut rng = Rng(0x1234_5678);
    let mut next: Vec<u64> = cams.iter_mut().map(|c| c.next_ts(&mut rng)).collect();
    let mut out = Vec::new();
    loop {
        let (cam, ts) = next
            .iter()
            .copied()
            .enumerate()
            .min_by_key(|&(_, ts)| ts)
            .unwrap();
        if ts > until {
            break;
        }
        next[cam] = cams[cam].next_ts(&mut rng);
        let now = ts + 5 * MS;
        if !skip(cam, ts) {
            grouper.push(cam, ts, now, Lease::new(live, ts));
        }
        grouper.poll(now);
        while let Some(g) = grouper.pop() {
            out.push(g);
        }
    }
    out
}

fn grouper(config: GroupConfig, cameras: usize) -> Grouper<Lease> {
    let mut g = Grouper::new(config);
    for _ in 0..cameras {
        g.add_camera();
    }
    g
}

#[test]
fn strict_groups_jittered_cameras_and_measures_spread() {
    let live = Rc::new(Cell::new(0));
    let mut g = grouper(GroupConfig::new(PERIOD / 2), 3);
    let mut cams = [
        SynthCam::new(0, PERIOD),
        SynthCam::new(2 * MS, PERIOD),
        SynthCam::new(4 * MS, PERIOD),
    ];
    for c in &mut cams {
        c.jitter = 500_000;
    }
    let groups = run(&mut g, &mut cams, 10 * PERIOD * 30, &live, |_, _| false);
    assert!(groups.len() >= 298, "{} groups", groups.len());
    for (i, group) in groups.iter().enumerate() {
        assert!(group.complete);
        assert_eq!(group.len(), 3);
        assert_eq!(group.sequence, i as u64);
        assert!(group.spread_ns <= 5 * MS, "spread {}", group.spread_ns);
        assert_eq!(group.cameras().collect::<Vec<_>>(), [0, 1, 2]);
        let offset = group.offset_ns(2).unwrap() - group.offset_ns(0).unwrap();
        assert!(
            (3 * MS as i64..=5 * MS as i64).contains(&offset),
            "{offset}"
        );
    }
    let report = g.report();
    assert_eq!(report.drops_total(), 0);
    // Every frame is grouped, or still pending at the end.
    let pending = (0..3).map(|c| g.pending(c) as u64).sum::<u64>();
    assert_eq!(report.frames_grouped + pending, report.frames_received);
    assert!(report.match_rate.unwrap() > 0.99);
    let p50 = report.spread_p50_ns.unwrap();
    assert!((3 * MS..=5 * MS).contains(&p50), "p50 {p50}");
    assert!(report.spread_p99_ns.unwrap() >= p50);
    assert!(report.spread_max_ns.unwrap() <= 5 * MS);
    let cam2 = &report.cameras[2];
    assert!((cam2.offset_mean_ns.unwrap() - 4e6).abs() < 2e5, "{cam2:?}");
    assert!(cam2.drift_ppm.unwrap().abs() < 10.0, "{cam2:?}");
    let period = report.cameras[0].period_ns.unwrap();
    assert!(period.abs_diff(PERIOD) < 200_000, "{period}");
    drop(groups);
    drop(g);
    assert_eq!(live.get(), 0);
}

#[test]
fn strict_drops_anchors_without_a_partner() {
    let live = Rc::new(Cell::new(0));
    let mut g = grouper(GroupConfig::new(3 * MS), 2);
    // Half a period apart: nothing is within 3 ms.
    let mut cams = [SynthCam::new(0, PERIOD), SynthCam::new(PERIOD / 2, PERIOD)];
    let groups = run(&mut g, &mut cams, 30 * PERIOD, &live, |_, _| false);
    assert!(groups.is_empty());
    let report = g.report();
    assert!(report.drops_of(DropReason::Unmatched) >= 55, "{report:?}");
    assert_eq!(report.match_rate, Some(0.0));
    assert!(g.held() <= 2 * 3);
}

#[test]
fn strict_waits_for_a_silent_camera_until_the_deadline() {
    let live = Rc::new(Cell::new(0));
    let mut g = grouper(GroupConfig::new(PERIOD / 2).deadline_ns(20 * MS), 2);
    g.push(0, 0, 0, Lease::new(&live, 0));
    assert_eq!(g.poll(MS), 0);
    assert_eq!(g.next_wake(), Some(20 * MS));
    assert_eq!(g.poll(19 * MS), 0);
    assert_eq!(g.pending(0), 1);
    assert_eq!(g.poll(20 * MS), 0);
    assert_eq!(g.pending(0), 0);
    assert_eq!(g.report().drops_of(DropReason::Incomplete), 1);
    assert_eq!(live.get(), 0, "the lease went back at once");
}

#[test]
fn partial_emits_what_it_has_at_the_deadline() {
    let live = Rc::new(Cell::new(0));
    let config = GroupConfig::new(PERIOD / 2)
        .policy(GroupPolicy::Partial)
        .deadline_ns(10 * MS);
    let mut g = grouper(config, 3);
    let mut cams = [
        SynthCam::new(0, PERIOD),
        SynthCam::new(MS, PERIOD),
        SynthCam::new(2 * MS, PERIOD),
    ];
    // Camera 2 loses every third frame.
    let groups = run(&mut g, &mut cams, 30 * PERIOD, &live, |cam, ts| {
        cam == 2 && (ts / PERIOD).is_multiple_of(3)
    });
    let partial: Vec<_> = groups.iter().filter(|g| !g.complete).collect();
    let complete = groups.len() - partial.len();
    assert!(partial.len() >= 9, "{} partial", partial.len());
    assert!(complete >= 18, "{complete} complete");
    for p in &partial {
        assert_eq!(p.cameras().collect::<Vec<_>>(), [0, 1]);
        assert!(p.get(2).is_none());
    }
    let report = g.report();
    assert_eq!(report.groups_partial, partial.len() as u64);
    assert_eq!(report.drops_total(), 0);
}

#[test]
fn partial_respects_min_members() {
    let live = Rc::new(Cell::new(0));
    let config = GroupConfig::new(MS)
        .policy(GroupPolicy::Partial)
        .deadline_ns(5 * MS)
        .min_members(2);
    let mut g = grouper(config, 2);
    g.push(0, 100 * MS, 100 * MS, Lease::new(&live, 0));
    assert_eq!(g.poll(200 * MS), 0, "a group of one is not enough");
    assert_eq!(g.report().drops_of(DropReason::Incomplete), 1);
    assert_eq!(live.get(), 0);
}

#[test]
fn latest_keeps_only_the_newest_complete_group() {
    let live = Rc::new(Cell::new(0));
    let config = GroupConfig::new(PERIOD / 2).policy(GroupPolicy::Latest);
    let mut g = grouper(config, 2);
    for n in 0..10u64 {
        let ts = n * PERIOD;
        g.push(0, ts, ts, Lease::new(&live, ts));
        g.push(1, ts + MS, ts + MS, Lease::new(&live, ts + MS));
        g.poll(ts + MS);
        // Nobody takes groups: each replaces the one before.
        assert!(g.ready() <= 1);
        assert!(g.held() <= 2 * (3 + 1));
    }
    let group = g.pop().unwrap();
    assert_eq!(group.get(0).unwrap().tag, 9 * PERIOD);
    assert_eq!(group.sequence, 9);
    let report = g.report();
    assert_eq!(report.groups_stale, 9);
    assert_eq!(report.drops_of(DropReason::Stale), 18);
    assert_eq!(report.frames_grouped, 2);
    drop(group);
    assert_eq!(live.get(), 0);
}

#[test]
fn slowest_pairs_a_fast_camera_to_the_slow_one() {
    let live = Rc::new(Cell::new(0));
    let config = GroupConfig::new(PERIOD / 4).rate(RateMatch::Slowest);
    let mut g = grouper(config, 2);
    // Camera 0 at 60 fps, camera 1 at 30 fps, 1 ms apart.
    let mut cams = [SynthCam::new(0, PERIOD / 2), SynthCam::new(MS, PERIOD)];
    let groups = run(&mut g, &mut cams, 60 * PERIOD, &live, |_, _| false);
    assert!(groups.len() >= 57, "{} groups", groups.len());
    for group in &groups {
        assert!(group.complete);
        assert!(group.spread_ns <= 2 * MS, "{}", group.spread_ns);
    }
    let report = g.report();
    // Every slow frame is grouped, half of the fast ones.
    let rate = report.match_rate.unwrap();
    assert!((0.6..0.7).contains(&rate), "{rate}");
    assert!(report.cameras[0].drops_of(DropReason::Unmatched) >= 55);
    assert_eq!(report.cameras[1].drops.iter().sum::<u64>(), 0);
}

#[test]
fn nearest_handles_different_rates_too() {
    let live = Rc::new(Cell::new(0));
    let mut g = grouper(GroupConfig::new(PERIOD / 4), 2);
    let mut cams = [SynthCam::new(0, PERIOD / 2), SynthCam::new(MS, PERIOD)];
    let groups = run(&mut g, &mut cams, 60 * PERIOD, &live, |_, _| false);
    assert!(groups.len() >= 57, "{} groups", groups.len());
    assert!(groups.iter().all(|g| g.spread_ns <= 2 * MS));
}

#[test]
fn drift_is_estimated_from_the_offsets() {
    let live = Rc::new(Cell::new(0));
    let mut g = grouper(GroupConfig::new(PERIOD / 2), 2);
    let mut cams = [SynthCam::new(0, PERIOD), SynthCam::new(0, PERIOD)];
    cams[1].drift_ppm = 50.0;
    cams[1].jitter = 20_000;
    cams[0].jitter = 20_000;
    // 60 s: the offset grows to 3 ms.
    let groups = run(&mut g, &mut cams, 60_000 * MS, &live, |_, _| false);
    assert!(groups.len() > 1700);
    let cam1 = &g.report().cameras[1];
    let ppm = cam1.drift_ppm.unwrap();
    assert!((ppm - 50.0).abs() < 2.0, "estimated {ppm} ppm");
    let offset = cam1.offset_ns.unwrap();
    assert!((2_900_000..3_100_000).contains(&offset), "{offset}");
    assert!(g.report().cameras[0].drift_ppm.is_none(), "the reference");
}

#[test]
fn drift_estimator_fits_a_line() {
    let mut d = DriftEstimator::new(64);
    assert_eq!(d.ppm(), None);
    for i in 0..1000u64 {
        let t = 1_000_000_000_000 + i * PERIOD;
        // -12 ppm: -12 ns per ms.
        d.push(t, 500 - (i * PERIOD / 1_000_000 * 12) as i64);
    }
    assert!((d.ppm().unwrap() + 12.0).abs() < 0.01, "{:?}", d.ppm());
    d.push(0, 0);
    assert_eq!(d.samples(), 1000, "time going back is ignored");
}

#[test]
fn late_frames_are_dropped() {
    let live = Rc::new(Cell::new(0));
    let config = GroupConfig::new(2 * MS)
        .policy(GroupPolicy::Partial)
        .deadline_ns(5 * MS)
        .min_members(1);
    let mut g = grouper(config, 2);
    g.push(0, 100 * MS, 100 * MS, Lease::new(&live, 0));
    g.push(0, 133 * MS, 133 * MS, Lease::new(&live, 0));
    g.poll(140 * MS);
    assert_eq!(g.ready(), 2);
    // Camera 1's frame for 100 ms shows up after groups past it went out.
    assert_eq!(
        g.push(1, 99 * MS, 141 * MS, Lease::new(&live, 1)),
        Pushed::Dropped(DropReason::Late)
    );
    // A timestamp going back on a camera is late too.
    assert_eq!(
        g.push(0, 120 * MS, 142 * MS, Lease::new(&live, 1)),
        Pushed::Dropped(DropReason::Late)
    );
    assert_eq!(g.report().drops_of(DropReason::Late), 2);
    assert_eq!(live.get(), 2, "only the two grouped frames are held");
}

#[test]
fn the_anchor_gives_way_to_its_next_frame() {
    let live = Rc::new(Cell::new(0));
    let mut g = grouper(GroupConfig::new(20 * MS), 2);
    g.push(0, 0, 0, Lease::new(&live, 0));
    g.push(0, 10 * MS, 10 * MS, Lease::new(&live, 10));
    g.push(1, 12 * MS, 12 * MS, Lease::new(&live, 12));
    g.poll(12 * MS);
    let group = g.pop().unwrap();
    assert_eq!(group.get(0).unwrap().tag, 10);
    assert_eq!(group.get(1).unwrap().tag, 12);
    assert_eq!(g.report().drops_of(DropReason::Superseded), 1);
}

#[test]
fn overflow_keeps_memory_bounded() {
    let live = Rc::new(Cell::new(0));
    let mut g = grouper(GroupConfig::new(MS).depth(2), 2);
    for n in 0..100u64 {
        let pushed = g.push(0, n * PERIOD, n * PERIOD, Lease::new(&live, n));
        if n >= 2 {
            assert_eq!(pushed, Pushed::Evicted);
        }
        g.poll(n * PERIOD);
        assert!(live.get() <= 2);
    }
    assert_eq!(g.pending(0), 2);
    assert_eq!(g.report().drops_of(DropReason::Overflow), 98);
}

#[test]
fn bounded_under_a_slow_consumer_and_back_to_zero() {
    let live = Rc::new(Cell::new(0));
    let config = GroupConfig::new(PERIOD / 2).depth(3).output_depth(2);
    let mut g = grouper(config, 4);
    let mut rng = Rng(7);
    let mut held_max = 0;
    for n in 0..1000u64 {
        for cam in 0..4 {
            let ts = n * PERIOD + cam as u64 * MS + (rng.jitter(100_000) + 100_000) as u64;
            g.push(cam, ts, ts, Lease::new(&live, ts));
        }
        g.poll(n * PERIOD + 5 * MS);
        held_max = held_max.max(live.get());
        // The consumer takes one group in ten.
        if n.is_multiple_of(10) {
            drop(g.pop());
        }
    }
    assert!(held_max <= 4 * (3 + 2), "held {held_max}");
    assert!(g.report().drops_of(DropReason::Stale) > 0);
    g.clear();
    assert_eq!(live.get(), 0);
    assert_eq!(g.held(), 0);
}

#[test]
fn cameras_disconnect_and_reconnect() {
    let live = Rc::new(Cell::new(0));
    let partial = GroupConfig::new(PERIOD / 2)
        .policy(GroupPolicy::Partial)
        .deadline_ns(10 * MS);
    let mut p = grouper(partial, 3);
    let mut s = grouper(GroupConfig::new(PERIOD / 2).deadline_ns(10 * MS), 3);
    let feed = |g: &mut Grouper<Lease>, n: u64, cams: &[usize]| {
        let ts = n * PERIOD;
        for &cam in cams {
            g.push(
                cam,
                ts + cam as u64 * MS,
                ts + 4 * MS,
                Lease::new(&live, ts),
            );
        }
        g.poll(ts + 4 * MS);
        let mut out = Vec::new();
        while let Some(group) = g.pop() {
            out.push((group.complete, group.len()));
        }
        out
    };
    for g in [&mut p, &mut s] {
        assert_eq!(feed(g, 0, &[0, 1, 2]), [(true, 3)]);
        // Camera 2 has a frame pending when it disconnects: it goes back.
        g.push(2, PERIOD + 2 * MS, PERIOD, Lease::new(&live, 0));
        g.set_connected(2, false);
        assert_eq!(g.report().drops_of(DropReason::Disconnected), 1);
        assert!(!g.is_connected(2));
    }
    // Partial carries on without it, without waiting; strict cannot.
    for n in 1..5 {
        assert_eq!(feed(&mut p, n, &[0, 1]), [(false, 2)]);
        assert!(feed(&mut s, n, &[0, 1]).is_empty());
    }
    assert!(s.report().drops_of(DropReason::Incomplete) >= 4);
    // Back: complete groups again (a frame reconnects it as well).
    p.set_connected(2, true);
    for n in 5..8 {
        assert_eq!(feed(&mut p, n, &[0, 1, 2]), [(true, 3)]);
        assert_eq!(feed(&mut s, n, &[0, 1, 2]), [(true, 3)]);
    }
    // Removed for good: groups without it are complete.
    p.remove_camera(2);
    assert_eq!(feed(&mut p, 8, &[0, 1]), [(true, 2)]);
    drop(p);
    drop(s);
    assert_eq!(live.get(), 0);
}

#[test]
fn latest_ignores_a_disconnected_camera() {
    let live = Rc::new(Cell::new(0));
    let mut g = grouper(GroupConfig::new(PERIOD / 2).policy(GroupPolicy::Latest), 3);
    g.set_connected(2, false);
    g.push(0, 0, 0, Lease::new(&live, 0));
    g.push(1, MS, MS, Lease::new(&live, 1));
    g.poll(MS);
    let group = g.pop().unwrap();
    assert_eq!(group.len(), 2);
    assert!(!group.complete);
}

#[test]
fn map_keeps_the_group() {
    let live = Rc::new(Cell::new(0));
    let mut g = grouper(GroupConfig::new(MS), 2);
    g.push(0, 10, 10, Lease::new(&live, 10));
    g.push(1, 20, 20, Lease::new(&live, 20));
    g.poll(20);
    let mut group = g.pop().unwrap().map(|camera, lease| (camera, lease.tag));
    assert_eq!(group.spread_ns, 10);
    assert_eq!(group.offset_ns(1), Some(10));
    assert_eq!(group.take(1), Some((1, 20)));
    assert_eq!(group.len(), 1);
    assert_eq!(live.get(), 0, "map consumed the leases");
}

#[test]
fn drop_reasons_are_named_once() {
    for (i, reason) in DropReason::ALL.iter().enumerate() {
        assert_eq!(reason.index(), i);
        assert!(
            DropReason::ALL[..i]
                .iter()
                .all(|r| r.as_str() != reason.as_str())
        );
    }
}
