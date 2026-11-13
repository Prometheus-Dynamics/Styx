//! The grouper: matches frames of up to `N` cameras by timestamp and hands out groups.

use alloc::vec::Vec;
use core::array;

use styx_frame::FrameLease;

use crate::config::{GroupConfig, GroupPolicy, RateMatch};
use crate::group::{CameraIndex, FrameGroup, Member};
use crate::reason::DropReason;
use crate::ring::Ring;
use crate::stats::Stats;

/// What [`Grouper::push`] did with a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pushed {
    /// Held for grouping.
    Accepted,
    /// Held; the camera's oldest pending frame was dropped ([`DropReason::Overflow`]).
    Evicted,
    /// Dropped at once (released before `push` returned).
    Dropped(DropReason),
}

/// Where one step left the matching.
#[derive(Clone, Copy, Debug)]
enum Step {
    Progress,
    /// Waiting for a frame until this arrival-timeline instant.
    Wait(u64),
    Idle,
}

#[derive(Debug)]
struct Pending<T> {
    ts: u64,
    arrived: u64,
    item: T,
}

#[derive(Debug)]
struct Lane<T> {
    pending: Ring<Pending<T>>,
    connected: bool,
    removed: bool,
    last_ts: Option<u64>,
    /// Smoothed frame period from the timestamps (0: not known yet).
    period_ns: u64,
}

impl<T> Lane<T> {
    fn new(depth: usize) -> Self {
        Self {
            pending: Ring::new(depth),
            connected: true,
            removed: false,
            last_ts: None,
            period_ns: 0,
        }
    }

    /// When the next frame is expected, from the last one and the period.
    fn next_expected(&self) -> Option<u64> {
        (self.period_ns > 0)
            .then_some(self.last_ts?)
            .map(|ts| ts.saturating_add(self.period_ns))
    }

    fn live(&self) -> bool {
        self.connected && !self.removed
    }
}

/// Groups frames of up to `N` cameras by timestamp; see the [crate docs](crate).
///
/// Every frame it holds sits in storage made by [`Grouper::new`] and the cameras' lanes: a push,
/// a poll and a pop make no allocation. Timestamps are on one clock the caller chose, and
/// `arrived_ns` on one timeline (the `now` passed to [`Grouper::poll`]).
#[derive(Debug)]
pub struct Grouper<T, const N: usize> {
    config: GroupConfig,
    cameras: usize,
    lanes: Vec<Lane<T>>,
    ready: Ring<FrameGroup<T, N>>,
    stats: Stats<N>,
    sequence: u64,
    last_anchor: Option<u64>,
    wake: Option<u64>,
}

impl<T, const N: usize> Grouper<T, N> {
    /// A grouper with no cameras yet; add them with [`Self::add_camera`]. Allocates its per-camera
    /// lanes and output slots here, once.
    pub fn new(mut config: GroupConfig) -> Self {
        config.depth = config.depth.max(1);
        config.output_depth = config.output_depth.max(1);
        config.min_members = config.min_members.max(1);
        let lanes = (0..N).map(|_| Lane::new(config.depth)).collect();
        Self {
            ready: Ring::new(config.output_depth),
            config,
            cameras: 0,
            lanes,
            stats: Stats::new(),
            sequence: 0,
            last_anchor: None,
            wake: None,
        }
    }

    pub fn config(&self) -> &GroupConfig {
        &self.config
    }

    /// Registers a camera (connected) and returns its index, or `None` when all `N` are in use.
    pub fn add_camera(&mut self) -> Option<CameraIndex> {
        if self.cameras == N {
            return None;
        }
        self.cameras += 1;
        Some(self.cameras - 1)
    }

    /// Cameras registered (removed ones included: indices are not reused).
    pub fn cameras(&self) -> usize {
        self.cameras
    }

    /// Removes `camera` for good: its pending frames are dropped, groups stop needing it.
    pub fn remove_camera(&mut self, camera: CameraIndex) {
        self.set_connected(camera, false);
        if let Some(lane) = self.lanes.get_mut(camera).filter(|_| camera < self.cameras) {
            lane.removed = true;
        }
    }

    /// Marks `camera` connected or not. Disconnecting drops its pending frames
    /// ([`DropReason::Disconnected`]) and forgets its timestamps (a reconnected camera may
    /// restart its clock); a disconnected camera is not waited for (but `Strict` still needs
    /// it). A frame pushed for a disconnected camera reconnects it.
    pub fn set_connected(&mut self, camera: CameraIndex, connected: bool) {
        if camera >= self.cameras {
            return;
        }
        let lane = &mut self.lanes[camera];
        if lane.removed {
            return;
        }
        if !connected {
            while lane.pending.pop_front().is_some() {
                self.stats.add_drop(camera, DropReason::Disconnected);
            }
            lane.last_ts = None;
        }
        lane.connected = connected;
    }

    pub fn is_connected(&self, camera: CameraIndex) -> bool {
        camera < self.cameras && self.lanes[camera].live()
    }

    /// The camera's measured frame period in nanoseconds (0 until two frames have arrived).
    pub fn period_ns(&self, camera: CameraIndex) -> u64 {
        if camera < self.cameras {
            self.lanes[camera].period_ns
        } else {
            0
        }
    }

    /// Takes a frame of `camera`: `timestamp_ns` on the grouper's clock, `arrived_ns` on the
    /// timeline of [`Self::poll`]'s `now`. Groups form in [`Self::poll`].
    pub fn push(
        &mut self,
        camera: CameraIndex,
        timestamp_ns: u64,
        arrived_ns: u64,
        item: T,
    ) -> Pushed {
        if camera >= self.cameras {
            return Pushed::Dropped(DropReason::Disconnected);
        }
        let tolerance = self.config.tolerance_ns;
        let behind_groups = self
            .last_anchor
            .is_some_and(|anchor| timestamp_ns.saturating_add(tolerance) < anchor);
        let lane = &mut self.lanes[camera];
        self.stats.add_received(camera);
        if lane.removed {
            self.stats.add_drop(camera, DropReason::Disconnected);
            return Pushed::Dropped(DropReason::Disconnected);
        }
        lane.connected = true;
        let backwards = lane.last_ts.is_some_and(|last| timestamp_ns <= last);
        if behind_groups || backwards {
            self.stats.add_drop(camera, DropReason::Late);
            return Pushed::Dropped(DropReason::Late);
        }
        if let Some(last) = lane.last_ts {
            let delta = timestamp_ns - last;
            lane.period_ns = match lane.period_ns {
                0 => delta,
                // A gap of n periods (dropped frames) counts as n periods of delta / n.
                period => {
                    let n = (delta.saturating_add(period / 2) / period).max(1);
                    let sample = i128::from(delta / n);
                    (i128::from(period) + (sample - i128::from(period)) / 8) as u64
                }
            };
        }
        lane.last_ts = Some(timestamp_ns);
        let mut outcome = Pushed::Accepted;
        if lane.pending.is_full() {
            lane.pending.pop_front();
            self.stats.add_drop(camera, DropReason::Overflow);
            outcome = Pushed::Evicted;
        }
        // Room was made above.
        let _ = lane.pending.push_back(Pending {
            ts: timestamp_ns,
            arrived: arrived_ns,
            item,
        });
        outcome
    }

    /// Counts a frame of `camera` dropped before it could be pushed (e.g. its clock could not
    /// be mapped: [`DropReason::Clock`]).
    pub fn count_drop(&mut self, camera: CameraIndex, reason: DropReason) {
        if camera < self.cameras {
            self.stats.add_received(camera);
            self.stats.add_drop(camera, reason);
        }
    }

    /// Forms every group it can as of `now` (arrival timeline); returns how many are ready.
    /// Call again at [`Self::next_wake`] when nothing new arrives before then.
    pub fn poll(&mut self, now: u64) -> usize {
        self.wake = None;
        loop {
            match self.step(now) {
                Step::Progress => {}
                Step::Wait(at) => {
                    self.wake = Some(at);
                    break;
                }
                Step::Idle => break,
            }
        }
        self.ready.len()
    }

    /// The next group, oldest first.
    pub fn pop(&mut self) -> Option<FrameGroup<T, N>> {
        self.ready.pop_front()
    }

    /// Groups ready.
    pub fn ready(&self) -> usize {
        self.ready.len()
    }

    /// When (arrival timeline) a waiting group's deadline passes: poll then.
    pub fn next_wake(&self) -> Option<u64> {
        self.wake
    }

    /// Frames pending for `camera`.
    pub fn pending(&self, camera: CameraIndex) -> usize {
        self.lanes
            .get(camera)
            .filter(|_| camera < self.cameras)
            .map_or(0, |lane| lane.pending.len())
    }

    /// Every frame the grouper holds: pending and in ready groups. At most
    /// `cameras × (depth + output_depth)`.
    pub fn held(&self) -> usize {
        let pending: usize = self.lanes[..self.cameras]
            .iter()
            .map(|lane| lane.pending.len())
            .sum();
        let ready: usize = (0..self.ready.len())
            .filter_map(|i| self.ready.get(i))
            .map(FrameGroup::len)
            .sum();
        pending + ready
    }

    /// Drops every pending frame and ready group (as [`DropReason::Stale`]).
    pub fn clear(&mut self) {
        for camera in 0..self.cameras {
            while self.lanes[camera].pending.pop_front().is_some() {
                self.stats.add_drop(camera, DropReason::Stale);
            }
        }
        while let Some(group) = self.ready.pop_front() {
            self.stale(group);
        }
    }

    /// Counters: frames per camera, drops per reason, groups and spread.
    pub fn stats(&self) -> &Stats<N> {
        &self.stats
    }

    /// Live camera with the oldest pending frame.
    fn oldest(&self) -> Option<CameraIndex> {
        self.lanes[..self.cameras]
            .iter()
            .enumerate()
            .filter(|(_, lane)| lane.live())
            .filter_map(|(i, lane)| Some((lane.pending.front()?.ts, i)))
            .min()
            .map(|(_, i)| i)
    }

    /// The anchor's camera, or how long to wait for one.
    fn anchor(&self, now: u64) -> Result<CameraIndex, Step> {
        let oldest = self.oldest().ok_or(Step::Idle)?;
        if self.config.rate == RateMatch::Nearest {
            return Ok(oldest);
        }
        // The slowest live camera with a measured period; on a tie, the lowest index.
        let mut pivot: Option<CameraIndex> = None;
        for (i, lane) in self.lanes[..self.cameras].iter().enumerate() {
            if lane.live()
                && lane.period_ns > 0
                && pivot.is_none_or(|p| lane.period_ns > self.lanes[p].period_ns)
            {
                pivot = Some(i);
            }
        }
        match pivot {
            None => Ok(oldest),
            Some(pivot) if !self.lanes[pivot].pending.is_empty() => Ok(pivot),
            // The slowest camera has nothing yet: anchor elsewhere only once a frame has
            // waited out the deadline (a stalled pivot must not stop everything).
            Some(_) => {
                let arrived = self.lanes[oldest]
                    .pending
                    .front()
                    .map_or(now, |p| p.arrived);
                let deadline = arrived.saturating_add(self.config.deadline_ns);
                if now >= deadline {
                    Ok(oldest)
                } else {
                    Err(Step::Wait(deadline))
                }
            }
        }
    }

    fn step(&mut self, now: u64) -> Step {
        let a = match self.anchor(now) {
            Ok(a) => a,
            Err(step) => return step,
        };
        let tolerance = self.config.tolerance_ns;
        let (a_ts, a_arrived) = match self.lanes[a].pending.front() {
            Some(front) => (front.ts, front.arrived),
            None => return Step::Idle,
        };
        // Frames too old for this anchor are too old for every later one.
        for camera in 0..self.cameras {
            if camera == a {
                continue;
            }
            while self.lanes[camera]
                .pending
                .front()
                .is_some_and(|p| p.ts.saturating_add(tolerance) < a_ts)
            {
                self.lanes[camera].pending.pop_front();
                self.stats.add_drop(camera, DropReason::Unmatched);
            }
        }
        let strict = self.config.policy == GroupPolicy::Strict;
        // Matches as (camera, position in its pending frames); at most one per camera.
        let mut chosen = [(0_usize, 0_usize); N];
        let mut chosen_len = 0;
        let (mut waiting, mut unmatched, mut absent) = (false, false, false);
        for index in 0..self.cameras {
            if index == a {
                continue;
            }
            let lane = &self.lanes[index];
            if lane.removed {
                continue;
            }
            if !lane.connected {
                absent |= strict;
                continue;
            }
            // Nearest pending frame to the anchor; on a tie, the older one.
            let mut best: Option<(u64, usize)> = None;
            for pos in 0..lane.pending.len() {
                if let Some(p) = lane.pending.get(pos) {
                    let distance = p.ts.abs_diff(a_ts);
                    if best.is_none_or(|(d, _)| distance < d) {
                        best = Some((distance, pos));
                    }
                }
            }
            let next = lane.next_expected();
            match best {
                Some((distance, pos)) if distance <= tolerance => {
                    let frame_ts = lane.pending.get(pos).map_or(a_ts, |p| p.ts);
                    let settled = frame_ts >= a_ts
                        || pos + 1 < lane.pending.len()
                        || next.is_none_or(|next| next.abs_diff(a_ts) > distance);
                    if settled {
                        chosen[chosen_len] = (index, pos);
                        chosen_len += 1;
                    } else {
                        waiting = true;
                    }
                }
                _ => {
                    let passed = lane.pending.back().is_some_and(|p| p.ts >= a_ts);
                    let next_too_late = next.is_some_and(|next| {
                        next > a_ts
                            .saturating_add(tolerance)
                            .saturating_add(lane.period_ns / 8)
                    });
                    if passed || next_too_late {
                        unmatched = true;
                    } else {
                        waiting = true;
                    }
                }
            }
        }
        let chosen = &chosen[..chosen_len];
        let deadline = a_arrived.saturating_add(self.config.deadline_ns);
        if waiting && now < deadline {
            return Step::Wait(deadline);
        }
        // The anchor's own next frame may suit a match better: then the anchor gives way.
        if let Some(next) = self.lanes[a].pending.get(1) {
            let closer = chosen.iter().any(|&(c, pos)| {
                self.lanes[c]
                    .pending
                    .get(pos)
                    .is_some_and(|p| next.ts.abs_diff(p.ts) < a_ts.abs_diff(p.ts))
            });
            if closer {
                self.drop_anchor(a, DropReason::Superseded);
                return Step::Progress;
            }
        }
        let missing = waiting || unmatched || absent;
        let live = self.lanes[..self.cameras]
            .iter()
            .filter(|lane| lane.live())
            .count();
        let enough = chosen_len + 1 >= self.config.min_members.min(live.max(1));
        let emit = match self.config.policy {
            GroupPolicy::Strict => !missing,
            GroupPolicy::Partial => enough,
            GroupPolicy::Latest => !missing && enough,
        };
        if emit {
            self.emit(a, chosen);
        } else {
            let reason = if unmatched {
                DropReason::Unmatched
            } else {
                DropReason::Incomplete
            };
            self.drop_anchor(a, reason);
        }
        Step::Progress
    }

    fn drop_anchor(&mut self, a: CameraIndex, reason: DropReason) {
        self.lanes[a].pending.pop_front();
        self.stats.add_drop(a, reason);
    }

    fn emit(&mut self, a: CameraIndex, chosen: &[(CameraIndex, usize)]) {
        let mut slots: [Option<Member<T>>; N] = array::from_fn(|_| None);
        let anchor = self.lanes[a]
            .pending
            .pop_front()
            .expect("the anchor has a pending frame");
        let reference_ns = anchor.ts;
        slots[a] = Some(Member {
            timestamp_ns: anchor.ts,
            item: anchor.item,
        });
        for &(c, pos) in chosen {
            for _ in 0..pos {
                self.lanes[c].pending.pop_front();
                self.stats.add_drop(c, DropReason::Superseded);
            }
            let frame = self.lanes[c]
                .pending
                .pop_front()
                .expect("the chosen frame is pending");
            slots[c] = Some(Member {
                timestamp_ns: frame.ts,
                item: frame.item,
            });
        }
        let complete = (0..self.cameras).all(|i| self.lanes[i].removed || slots[i].is_some());
        for (camera, slot) in slots.iter().enumerate() {
            if slot.is_some() {
                self.stats.add_grouped(camera);
            }
        }
        let group = FrameGroup::from_slots(self.sequence, reference_ns, complete, slots);
        self.sequence += 1;
        self.last_anchor = Some(reference_ns);
        self.stats.add_group(group.spread_ns, complete);
        let limit = match self.config.policy {
            GroupPolicy::Latest => 1,
            _ => self.config.output_depth,
        };
        while self.ready.len() >= limit {
            if let Some(old) = self.ready.pop_front() {
                self.stale(old);
            }
        }
        // Room was made above.
        let _ = self.ready.push_back(group);
    }

    fn stale(&mut self, group: FrameGroup<T, N>) {
        for camera in group.cameras() {
            self.stats.remove_grouped(camera);
            self.stats.add_drop(camera, DropReason::Stale);
        }
        self.stats.add_stale_group();
    }
}

impl<M, const N: usize> Grouper<FrameLease<M>, N> {
    /// Takes a Styx frame: its timestamp (`FrameLease::timestamp_ns`, CLOCK_MONOTONIC) is the
    /// grouper's clock. The lease is held until it is grouped, dropped, or taken out of a group.
    pub fn push_frame(
        &mut self,
        camera: CameraIndex,
        frame: FrameLease<M>,
        arrived_ns: u64,
    ) -> Pushed {
        let timestamp_ns = frame.timestamp_ns();
        self.push(camera, timestamp_ns, arrived_ns, frame)
    }
}
