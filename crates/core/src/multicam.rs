//! Multi-camera frame grouping by sensor timestamp (`no_std` + `alloc`).
//!
//! A [`Grouper`] takes frames from N cameras, each with a timestamp on one common clock (the
//! caller maps clocks first: `styx::multicam` does, from `FrameMeta::timestamp` and
//! `FrameMeta::clock`), and hands out [`FrameGroup`]s: one frame per camera whose timestamps lie
//! within [`GroupConfig::tolerance_ns`] of the group's anchor. It holds at most
//! [`GroupConfig::depth`] frames per camera and [`GroupConfig::output_depth`] groups, dropping
//! (never copying) the rest, so a capture pool behind it never starves; every drop is counted by
//! [`DropReason`]. [`SyncStats`] keeps the sync quality: timestamp spread per group and over a
//! window (p50/p99), each camera's offset to a reference camera and its drift, match rate.
//!
//! The algorithm is pure: it never reads a clock. The caller passes when each frame arrived and
//! the time now (one monotonic nanosecond timeline) for deadlines, and polls again at
//! [`Grouper::next_wake`].
//!
//! - **Anchor.** Each step takes an anchor frame: the oldest pending frame of any connected
//!   camera ([`RateMatch::Nearest`]), or the oldest of the slowest camera's
//!   ([`RateMatch::Slowest`], measured from each camera's timestamps), so a 60 fps camera paired
//!   with a 30 fps one contributes every other frame.
//! - **Match.** Every other camera contributes its frame nearest to the anchor, once that choice
//!   is settled (a frame at or after the anchor, or the camera's next frame, predicted from its
//!   frame period, would be farther). Frames older than a camera's chosen one are dropped
//!   ([`DropReason::Superseded`]). When the anchor's own next frame is nearer to a match, the
//!   anchor gives way (`Superseded`).
//! - **Policy.** [`GroupPolicy::Strict`] needs every registered camera; [`GroupPolicy::Partial`]
//!   emits what it has at the deadline (at least [`GroupConfig::min_members`]);
//!   [`GroupPolicy::Latest`] keeps only the newest complete group of the connected cameras.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use smallvec::SmallVec;

mod stats;
pub use stats::{CameraSyncReport, DriftEstimator, SPREAD_WINDOW, SyncReport, SyncStats};

/// A camera's index in a [`Grouper`]: the order [`Grouper::add_camera`] gave it. Stable for
/// the grouper's life (a removed camera's index is not reused).
pub type CameraIndex = usize;

/// What a group needs to be emitted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GroupPolicy {
    /// Every registered camera (connected or not) contributes a frame, or the anchor is dropped.
    /// Groups are queued in order ([`GroupConfig::output_depth`]).
    #[default]
    Strict,
    /// Best effort: wait up to [`GroupConfig::deadline_ns`] for the connected cameras, then emit
    /// the frames that matched (at least [`GroupConfig::min_members`]; disconnected cameras are
    /// not waited for). [`FrameGroup::complete`] tells full groups apart.
    Partial,
    /// The newest group of every connected camera: a newer group replaces one not taken yet
    /// ([`DropReason::Stale`]), so the consumer always gets the freshest synchronized set.
    Latest,
}

impl GroupPolicy {
    /// Stable snake-case name (metric labels, logs).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Partial => "partial",
            Self::Latest => "latest",
        }
    }
}

/// How cameras at different frame rates pair up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RateMatch {
    /// Anchor on the oldest pending frame of any camera, pair to the nearest frames. Right for
    /// cameras at one rate; at different rates the faster camera's extra frames find no match.
    #[default]
    Nearest,
    /// Anchor on the slowest camera (largest measured frame period): one group per slowest
    /// frame, the faster cameras' frames in between dropped as [`DropReason::Unmatched`].
    Slowest,
}

/// Why a frame left a [`Grouper`] without being in a group handed out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DropReason {
    /// Another camera has frames, but none within the tolerance of this one.
    Unmatched,
    /// A frame of the same camera nearer to the group was used instead, or an older frame was
    /// passed over by the one chosen.
    Superseded,
    /// A camera needed for the group sent nothing in time (deadline) or is disconnected.
    Incomplete,
    /// The camera's pending frames were full ([`GroupConfig::depth`]): the oldest went.
    Overflow,
    /// The frame arrived after groups past its time were emitted (or went back in time).
    Late,
    /// In a group nobody took before a newer one replaced it (or the output was full).
    Stale,
    /// Its timestamp cannot be put on the grouper's clock (unknown or unrelated clock).
    Clock,
    /// Pending when its camera disconnected or was removed.
    Disconnected,
}

impl DropReason {
    /// How many reasons there are.
    pub const COUNT: usize = 8;

    /// Every reason, in declaration order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Unmatched,
        Self::Superseded,
        Self::Incomplete,
        Self::Overflow,
        Self::Late,
        Self::Stale,
        Self::Clock,
        Self::Disconnected,
    ];

    /// Position in [`Self::ALL`] (and in the per-reason counters).
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Stable snake-case name, used as a metric label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unmatched => "unmatched",
            Self::Superseded => "superseded",
            Self::Incomplete => "incomplete",
            Self::Overflow => "overflow",
            Self::Late => "late",
            Self::Stale => "stale",
            Self::Clock => "clock",
            Self::Disconnected => "disconnected",
        }
    }
}

/// Settings of a [`Grouper`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupConfig {
    /// Largest distance from the anchor's timestamp a member may have. About half the frame
    /// period for free-running cameras; tens of microseconds for hardware-synchronized ones.
    pub tolerance_ns: u64,
    pub policy: GroupPolicy,
    pub rate: RateMatch,
    /// How long (after the anchor frame arrived) a group waits for a camera whose frame is not
    /// there yet: then `Partial` emits without it, `Strict` and `Latest` drop the anchor.
    pub deadline_ns: u64,
    /// Frames held per camera (at least 1); a camera with this many pending loses its oldest.
    pub depth: usize,
    /// Groups held until taken (at least 1); `Latest` holds one.
    pub output_depth: usize,
    /// `Partial` and `Latest`: fewest frames a group may have (clamped to the cameras there).
    pub min_members: usize,
    /// Camera that offsets and drift are measured against (`None`: the first camera).
    pub reference: Option<CameraIndex>,
    /// Groups over which the drift estimate averages (exponential weighting).
    pub drift_window: u32,
}

impl GroupConfig {
    /// `tolerance_ns` with the defaults: strict, nearest, 100 ms deadline, 3 frames per camera,
    /// 2 groups queued, groups of at least 2, the first camera as reference, drift over 512
    /// groups.
    pub const fn new(tolerance_ns: u64) -> Self {
        Self {
            tolerance_ns,
            policy: GroupPolicy::Strict,
            rate: RateMatch::Nearest,
            deadline_ns: 100_000_000,
            depth: 3,
            output_depth: 2,
            min_members: 2,
            reference: None,
            drift_window: 512,
        }
    }

    pub const fn policy(mut self, policy: GroupPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub const fn rate(mut self, rate: RateMatch) -> Self {
        self.rate = rate;
        self
    }

    pub const fn deadline_ns(mut self, deadline_ns: u64) -> Self {
        self.deadline_ns = deadline_ns;
        self
    }

    pub const fn depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    pub const fn output_depth(mut self, output_depth: usize) -> Self {
        self.output_depth = output_depth;
        self
    }

    pub const fn min_members(mut self, min_members: usize) -> Self {
        self.min_members = min_members;
        self
    }

    pub const fn reference(mut self, camera: CameraIndex) -> Self {
        self.reference = Some(camera);
        self
    }

    pub const fn drift_window(mut self, groups: u32) -> Self {
        self.drift_window = groups;
        self
    }
}

/// One camera's frame in a [`FrameGroup`].
#[derive(Debug)]
pub struct Member<T> {
    pub camera: CameraIndex,
    /// On the grouper's clock.
    pub timestamp_ns: u64,
    pub item: T,
}

/// Frames of several cameras taken at (about) the same time.
#[derive(Debug)]
pub struct FrameGroup<T> {
    /// Groups handed out before this one by its grouper (0 first), gaps where groups went stale.
    pub sequence: u64,
    /// The anchor frame's timestamp: what the members were matched to.
    pub reference_ns: u64,
    /// Latest minus earliest member timestamp.
    pub spread_ns: u64,
    /// Every registered camera is in it.
    pub complete: bool,
    /// Ordered by camera.
    pub members: SmallVec<[Member<T>; 4]>,
}

impl<T> FrameGroup<T> {
    /// `camera`'s member.
    pub fn member(&self, camera: CameraIndex) -> Option<&Member<T>> {
        self.members.iter().find(|m| m.camera == camera)
    }

    /// `camera`'s frame.
    pub fn get(&self, camera: CameraIndex) -> Option<&T> {
        self.member(camera).map(|m| &m.item)
    }

    /// Takes `camera`'s frame out of the group.
    pub fn take(&mut self, camera: CameraIndex) -> Option<T> {
        let at = self.members.iter().position(|m| m.camera == camera)?;
        Some(self.members.remove(at).item)
    }

    /// `camera`'s timestamp minus the reference.
    pub fn offset_ns(&self, camera: CameraIndex) -> Option<i64> {
        self.member(camera)
            .map(|m| signed_diff(m.timestamp_ns, self.reference_ns))
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Cameras of the group, in order.
    pub fn cameras(&self) -> impl Iterator<Item = CameraIndex> + '_ {
        self.members.iter().map(|m| m.camera)
    }

    /// The same group over other items (e.g. frames turned into graph payloads).
    pub fn map<U>(self, mut f: impl FnMut(CameraIndex, T) -> U) -> FrameGroup<U> {
        FrameGroup {
            sequence: self.sequence,
            reference_ns: self.reference_ns,
            spread_ns: self.spread_ns,
            complete: self.complete,
            members: self
                .members
                .into_iter()
                .map(|m| Member {
                    camera: m.camera,
                    timestamp_ns: m.timestamp_ns,
                    item: f(m.camera, m.item),
                })
                .collect(),
        }
    }
}

pub(crate) fn signed_diff(a: u64, b: u64) -> i64 {
    (i128::from(a) - i128::from(b)).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

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

struct Pending<T> {
    ts: u64,
    arrived: u64,
    item: T,
}

struct Camera<T> {
    pending: VecDeque<Pending<T>>,
    connected: bool,
    removed: bool,
    last_ts: Option<u64>,
    /// Smoothed frame period from the timestamps (0: not known yet).
    period_ns: u64,
}

impl<T> Camera<T> {
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

/// Where one step left the matching.
enum Step {
    Progress,
    /// Waiting for a frame until this arrival-timeline instant.
    Wait(u64),
    Idle,
}

/// Groups frames of several cameras by timestamp; see the [module docs](self).
pub struct Grouper<T> {
    config: GroupConfig,
    cameras: Vec<Camera<T>>,
    ready: VecDeque<FrameGroup<T>>,
    stats: SyncStats,
    sequence: u64,
    last_anchor: Option<u64>,
    wake: Option<u64>,
}

impl<T> Grouper<T> {
    /// A grouper without cameras; add them with [`Self::add_camera`].
    pub fn new(mut config: GroupConfig) -> Self {
        config.depth = config.depth.max(1);
        config.output_depth = config.output_depth.max(1);
        config.min_members = config.min_members.max(1);
        let stats = SyncStats::new(config.drift_window);
        Self {
            config,
            cameras: Vec::new(),
            ready: VecDeque::new(),
            stats,
            sequence: 0,
            last_anchor: None,
            wake: None,
        }
    }

    pub fn config(&self) -> &GroupConfig {
        &self.config
    }

    /// Registers a camera (connected) and returns its index.
    pub fn add_camera(&mut self) -> CameraIndex {
        self.cameras.push(Camera {
            pending: VecDeque::with_capacity(self.config.depth),
            connected: true,
            removed: false,
            last_ts: None,
            period_ns: 0,
        });
        self.stats.add_camera();
        self.cameras.len() - 1
    }

    /// Cameras registered (removed ones included: indices are not reused).
    pub fn cameras(&self) -> usize {
        self.cameras.len()
    }

    /// Removes `camera` for good: its pending frames are dropped, groups stop needing it.
    pub fn remove_camera(&mut self, camera: CameraIndex) {
        self.set_connected(camera, false);
        if let Some(cam) = self.cameras.get_mut(camera) {
            cam.removed = true;
        }
    }

    /// Marks `camera` connected or not. Disconnecting drops its pending frames
    /// ([`DropReason::Disconnected`]) and forgets its timestamps (a reconnected camera may
    /// restart its clock); a disconnected camera is not waited for (but `Strict` still needs
    /// it). A frame pushed for a disconnected camera reconnects it.
    pub fn set_connected(&mut self, camera: CameraIndex, connected: bool) {
        let Some(cam) = self.cameras.get_mut(camera) else {
            return;
        };
        if cam.removed {
            return;
        }
        if !connected {
            while cam.pending.pop_front().is_some() {
                self.stats.dropped(camera, DropReason::Disconnected);
            }
            cam.last_ts = None;
        }
        cam.connected = connected;
    }

    pub fn is_connected(&self, camera: CameraIndex) -> bool {
        self.cameras.get(camera).is_some_and(Camera::live)
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
        let tolerance = self.config.tolerance_ns;
        let Some(cam) = self.cameras.get_mut(camera) else {
            return Pushed::Dropped(DropReason::Disconnected);
        };
        if cam.removed {
            drop(item);
            self.stats.received(camera);
            self.stats.dropped(camera, DropReason::Disconnected);
            return Pushed::Dropped(DropReason::Disconnected);
        }
        cam.connected = true;
        self.stats.received(camera);
        let behind_groups = self
            .last_anchor
            .is_some_and(|anchor| timestamp_ns.saturating_add(tolerance) < anchor);
        let backwards = cam.last_ts.is_some_and(|last| timestamp_ns <= last);
        if behind_groups || backwards {
            drop(item);
            self.stats.dropped(camera, DropReason::Late);
            return Pushed::Dropped(DropReason::Late);
        }
        if let Some(last) = cam.last_ts {
            let delta = timestamp_ns - last;
            cam.period_ns = match cam.period_ns {
                0 => delta,
                // A gap of n periods (dropped frames) counts as n periods of delta / n.
                period => {
                    let n = ((delta + period / 2) / period).max(1);
                    let sample = (delta / n) as i128;
                    (period as i128 + (sample - period as i128) / 8) as u64
                }
            };
        }
        cam.last_ts = Some(timestamp_ns);
        self.stats.set_period(camera, cam.period_ns);
        let mut outcome = Pushed::Accepted;
        if cam.pending.len() >= self.config.depth {
            cam.pending.pop_front();
            self.stats.dropped(camera, DropReason::Overflow);
            outcome = Pushed::Evicted;
        }
        cam.pending.push_back(Pending {
            ts: timestamp_ns,
            arrived: arrived_ns,
            item,
        });
        outcome
    }

    /// Counts a frame of `camera` dropped before it could be pushed (e.g. its clock could not
    /// be mapped: [`DropReason::Clock`]).
    pub fn count_drop(&mut self, camera: CameraIndex, reason: DropReason) {
        if camera < self.cameras.len() {
            self.stats.received(camera);
            self.stats.dropped(camera, reason);
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
    pub fn pop(&mut self) -> Option<FrameGroup<T>> {
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
        self.cameras.get(camera).map_or(0, |c| c.pending.len())
    }

    /// Every frame the grouper holds: pending and in ready groups. At most
    /// `cameras × (depth + output_depth)`.
    pub fn held(&self) -> usize {
        self.cameras.iter().map(|c| c.pending.len()).sum::<usize>()
            + self.ready.iter().map(FrameGroup::len).sum::<usize>()
    }

    /// Drops every pending frame and ready group (as [`DropReason::Stale`]).
    pub fn clear(&mut self) {
        for (index, cam) in self.cameras.iter_mut().enumerate() {
            while cam.pending.pop_front().is_some() {
                self.stats.dropped(index, DropReason::Stale);
            }
        }
        while let Some(group) = self.ready.pop_front() {
            self.stale(group);
        }
    }

    /// The counters behind [`Self::report`].
    pub fn stats(&self) -> &SyncStats {
        &self.stats
    }

    /// The sync quality now: groups, spread percentiles, offsets, drift, drops.
    pub fn report(&self) -> SyncReport {
        self.stats
            .report(|camera| self.cameras.get(camera).is_some_and(Camera::live))
    }

    fn reference_camera(&self) -> Option<CameraIndex> {
        match self.config.reference {
            Some(camera) if camera < self.cameras.len() => Some(camera),
            _ => self.cameras.iter().position(|c| !c.removed),
        }
    }

    /// Live camera with the oldest pending frame.
    fn oldest(&self) -> Option<CameraIndex> {
        self.cameras
            .iter()
            .enumerate()
            .filter(|(_, c)| c.live())
            .filter_map(|(i, c)| Some((c.pending.front()?.ts, i)))
            .min()
            .map(|(_, i)| i)
    }

    /// The anchor's camera, or how long to wait for one.
    fn anchor(&self, now: u64) -> Result<CameraIndex, Step> {
        let oldest = self.oldest().ok_or(Step::Idle)?;
        if self.config.rate == RateMatch::Nearest {
            return Ok(oldest);
        }
        let pivot = self
            .cameras
            .iter()
            .enumerate()
            .filter(|(_, c)| c.live() && c.period_ns > 0)
            .max_by_key(|(i, c)| (c.period_ns, core::cmp::Reverse(*i)))
            .map(|(i, _)| i);
        match pivot {
            None => Ok(oldest),
            Some(pivot) if !self.cameras[pivot].pending.is_empty() => Ok(pivot),
            // The slowest camera has nothing yet: anchor elsewhere only once a frame has
            // waited out the deadline (a stalled pivot must not stop everything).
            Some(_) => {
                let arrived = self.cameras[oldest]
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
        let (a_ts, a_arrived) = {
            let front = &self.cameras[a].pending[0];
            (front.ts, front.arrived)
        };
        // Frames too old for this anchor are too old for every later one.
        for (index, cam) in self.cameras.iter_mut().enumerate() {
            while cam
                .pending
                .front()
                .is_some_and(|p| index != a && p.ts.saturating_add(tolerance) < a_ts)
            {
                cam.pending.pop_front();
                self.stats.dropped(index, DropReason::Unmatched);
            }
        }
        let strict = self.config.policy == GroupPolicy::Strict;
        let mut chosen: SmallVec<[(CameraIndex, usize); 4]> = SmallVec::new();
        let (mut waiting, mut unmatched, mut absent) = (false, false, false);
        for (index, cam) in self.cameras.iter().enumerate() {
            if index == a || cam.removed {
                continue;
            }
            if !cam.connected {
                absent |= strict;
                continue;
            }
            let best = cam
                .pending
                .iter()
                .enumerate()
                .map(|(pos, p)| (p.ts.abs_diff(a_ts), pos))
                .min();
            let next = cam.next_expected();
            match best {
                Some((distance, pos)) if distance <= tolerance => {
                    let settled = cam.pending[pos].ts >= a_ts
                        || pos + 1 < cam.pending.len()
                        || next.is_none_or(|next| next.abs_diff(a_ts) > distance);
                    if settled {
                        chosen.push((index, pos));
                    } else {
                        waiting = true;
                    }
                }
                _ => {
                    let passed = cam.pending.back().is_some_and(|p| p.ts >= a_ts);
                    let next_too_late = next.is_some_and(|next| {
                        next > a_ts
                            .saturating_add(tolerance)
                            .saturating_add(cam.period_ns / 8)
                    });
                    if passed || next_too_late {
                        unmatched = true;
                    } else {
                        waiting = true;
                    }
                }
            }
        }
        let deadline = a_arrived.saturating_add(self.config.deadline_ns);
        if waiting && now < deadline {
            return Step::Wait(deadline);
        }
        // The anchor's own next frame may suit a match better: then the anchor gives way.
        if let Some(next) = self.cameras[a].pending.get(1) {
            let closer = chosen.iter().any(|&(c, pos)| {
                let ts = self.cameras[c].pending[pos].ts;
                next.ts.abs_diff(ts) < a_ts.abs_diff(ts)
            });
            if closer {
                self.drop_anchor(a, DropReason::Superseded);
                return Step::Progress;
            }
        }
        let missing = waiting || unmatched || absent;
        let live = self.cameras.iter().filter(|c| c.live()).count();
        let enough = chosen.len() + 1 >= self.config.min_members.min(live.max(1));
        let emit = match self.config.policy {
            GroupPolicy::Strict => !missing,
            GroupPolicy::Partial => enough,
            GroupPolicy::Latest => !missing && enough,
        };
        if emit {
            self.emit(a, &chosen);
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
        self.cameras[a].pending.pop_front();
        self.stats.dropped(a, reason);
    }

    fn emit(&mut self, a: CameraIndex, chosen: &[(CameraIndex, usize)]) {
        let mut members: SmallVec<[Member<T>; 4]> = SmallVec::new();
        let anchor = self.cameras[a]
            .pending
            .pop_front()
            .expect("anchor is pending");
        let reference_ns = anchor.ts;
        members.push(Member {
            camera: a,
            timestamp_ns: anchor.ts,
            item: anchor.item,
        });
        for &(c, pos) in chosen {
            let cam = &mut self.cameras[c];
            for _ in 0..pos {
                cam.pending.pop_front();
                self.stats.dropped(c, DropReason::Superseded);
            }
            let frame = cam.pending.pop_front().expect("chosen frame is pending");
            members.push(Member {
                camera: c,
                timestamp_ns: frame.ts,
                item: frame.item,
            });
        }
        members.sort_unstable_by_key(|m| m.camera);
        let (min, max) = members.iter().fold((u64::MAX, 0), |(lo, hi), m| {
            (lo.min(m.timestamp_ns), hi.max(m.timestamp_ns))
        });
        let complete = self
            .cameras
            .iter()
            .enumerate()
            .all(|(i, c)| c.removed || members.iter().any(|m| m.camera == i));
        let group = FrameGroup {
            sequence: self.sequence,
            reference_ns,
            spread_ns: max - min,
            complete,
            members,
        };
        self.sequence += 1;
        self.last_anchor = Some(reference_ns);
        self.stats.grouped(&group, self.reference_camera());
        let limit = match self.config.policy {
            GroupPolicy::Latest => 1,
            _ => self.config.output_depth,
        };
        while self.ready.len() >= limit {
            if let Some(old) = self.ready.pop_front() {
                self.stale(old);
            }
        }
        self.ready.push_back(group);
    }

    fn stale(&mut self, group: FrameGroup<T>) {
        for member in &group.members {
            self.stats.dropped(member.camera, DropReason::Stale);
        }
        self.stats.unhanded(&group);
    }
}

#[cfg(test)]
mod tests;
