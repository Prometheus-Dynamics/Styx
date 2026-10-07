//! Sync quality of a [`Grouper`](super::Grouper): spread per group and over a window, each
//! camera's offset to the reference camera and its drift, match rate, drops by reason.

use alloc::vec::Vec;

use super::{CameraIndex, DropReason, FrameGroup, signed_diff};

/// Groups whose spread the window percentiles cover.
pub const SPREAD_WINDOW: usize = 256;

/// The slope of a camera's offset over time (its clock drift against the reference camera),
/// by exponentially weighted least squares: no sample buffer, constant work per sample. The
/// time axis is re-centred on the newest sample at every update, so the sums stay small and
/// exact enough over any run length.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DriftEstimator {
    decay: f64,
    last_ns: Option<u64>,
    samples: u64,
    // Weighted sums of 1, x (seconds before the newest sample, <= 0), y (offset ns), x², xy.
    w: f64,
    x: f64,
    y: f64,
    xx: f64,
    xy: f64,
}

impl DriftEstimator {
    /// Averaging over about `window` samples (at least 2).
    pub fn new(window: u32) -> Self {
        Self {
            decay: 1.0 - 1.0 / f64::from(window.max(2)),
            ..Self::default()
        }
    }

    /// Records `offset_ns` measured at `at_ns` (the reference camera's timestamp).
    pub fn push(&mut self, at_ns: u64, offset_ns: i64) {
        if let Some(last) = self.last_ns {
            if at_ns <= last {
                return;
            }
            // Move the origin to `at_ns`: every earlier x shifts by -dt.
            let dt = (at_ns - last) as f64 * 1e-9;
            self.xx += -2.0 * dt * self.x + dt * dt * self.w;
            self.xy -= dt * self.y;
            self.x -= dt * self.w;
        }
        let d = self.decay;
        let y = offset_ns as f64;
        self.w = self.w * d + 1.0;
        self.x *= d;
        self.y = self.y * d + y;
        self.xx *= d;
        self.xy *= d;
        self.last_ns = Some(at_ns);
        self.samples += 1;
    }

    /// Samples recorded.
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Offset change per second of reference time, in nanoseconds (`None`: fewer than 3
    /// samples, or all at one instant).
    pub fn slope_ns_per_s(&self) -> Option<f64> {
        if self.samples < 3 {
            return None;
        }
        let det = self.w * self.xx - self.x * self.x;
        if det.abs() <= f64::EPSILON * self.w * self.xx.abs().max(1e-30) {
            return None;
        }
        Some((self.w * self.xy - self.x * self.y) / det)
    }

    /// [`Self::slope_ns_per_s`] in parts per million (1 ppm = 1 µs per second).
    pub fn ppm(&self) -> Option<f64> {
        self.slope_ns_per_s().map(|s| s / 1000.0)
    }

    /// The weighted mean offset, ns.
    pub fn mean_offset_ns(&self) -> Option<f64> {
        (self.w > 0.0).then(|| self.y / self.w)
    }
}

#[derive(Clone, Debug, PartialEq)]
struct CameraStats {
    received: u64,
    grouped: u64,
    drops: [u64; DropReason::COUNT],
    offset_ns: Option<i64>,
    period_ns: u64,
    drift: DriftEstimator,
}

/// The counters of a grouper: kept as groups form, read with [`SyncStats::report`].
#[derive(Clone, Debug, PartialEq)]
pub struct SyncStats {
    drift_window: u32,
    groups_complete: u64,
    groups_partial: u64,
    groups_stale: u64,
    cameras: Vec<CameraStats>,
    spreads: Vec<u64>,
    spread_next: usize,
    spread_last: Option<u64>,
    spread_max_ever: u64,
}

impl SyncStats {
    pub(super) fn new(drift_window: u32) -> Self {
        Self {
            drift_window,
            groups_complete: 0,
            groups_partial: 0,
            groups_stale: 0,
            cameras: Vec::new(),
            spreads: Vec::with_capacity(SPREAD_WINDOW),
            spread_next: 0,
            spread_last: None,
            spread_max_ever: 0,
        }
    }

    pub(super) fn add_camera(&mut self) {
        self.cameras.push(CameraStats {
            received: 0,
            grouped: 0,
            drops: [0; DropReason::COUNT],
            offset_ns: None,
            period_ns: 0,
            drift: DriftEstimator::new(self.drift_window),
        });
    }

    pub(super) fn received(&mut self, camera: CameraIndex) {
        if let Some(c) = self.cameras.get_mut(camera) {
            c.received += 1;
        }
    }

    pub(super) fn dropped(&mut self, camera: CameraIndex, reason: DropReason) {
        if let Some(c) = self.cameras.get_mut(camera) {
            c.drops[reason.index()] += 1;
        }
    }

    pub(super) fn set_period(&mut self, camera: CameraIndex, period_ns: u64) {
        if let Some(c) = self.cameras.get_mut(camera) {
            c.period_ns = period_ns;
        }
    }

    pub(super) fn grouped<T>(&mut self, group: &FrameGroup<T>, reference: Option<CameraIndex>) {
        if group.complete {
            self.groups_complete += 1;
        } else {
            self.groups_partial += 1;
        }
        for m in &group.members {
            if let Some(c) = self.cameras.get_mut(m.camera) {
                c.grouped += 1;
            }
        }
        if self.spreads.len() < SPREAD_WINDOW {
            self.spreads.push(group.spread_ns);
        } else {
            self.spreads[self.spread_next] = group.spread_ns;
        }
        self.spread_next = (self.spread_next + 1) % SPREAD_WINDOW;
        self.spread_last = Some(group.spread_ns);
        self.spread_max_ever = self.spread_max_ever.max(group.spread_ns);
        let Some(reference) = reference.and_then(|r| group.member(r)) else {
            return;
        };
        let at = reference.timestamp_ns;
        for m in &group.members {
            if let Some(c) = self.cameras.get_mut(m.camera) {
                let offset = signed_diff(m.timestamp_ns, at);
                c.offset_ns = Some(offset);
                if m.camera != reference.camera {
                    c.drift.push(at, offset);
                }
            }
        }
    }

    /// A group handed out was replaced before anyone took it: its frames count as dropped
    /// (`Stale`), not grouped.
    pub(super) fn unhanded<T>(&mut self, group: &FrameGroup<T>) {
        self.groups_stale += 1;
        for m in &group.members {
            if let Some(c) = self.cameras.get_mut(m.camera) {
                c.grouped = c.grouped.saturating_sub(1);
            }
        }
    }

    /// Frames received from every camera.
    pub fn frames_received(&self) -> u64 {
        self.cameras.iter().map(|c| c.received).sum()
    }

    /// Frames in groups handed out (or ready).
    pub fn frames_grouped(&self) -> u64 {
        self.cameras.iter().map(|c| c.grouped).sum()
    }

    /// Frames dropped for `reason`, every camera.
    pub fn drops(&self, reason: DropReason) -> u64 {
        self.cameras.iter().map(|c| c.drops[reason.index()]).sum()
    }

    /// The sync quality now. `connected` tells which cameras are connected.
    pub fn report(&self, connected: impl Fn(CameraIndex) -> bool) -> SyncReport {
        let mut spreads = self.spreads.clone();
        spreads.sort_unstable();
        let quantile = |q: f64| {
            let i = ((spreads.len().saturating_sub(1)) as f64 * q + 0.5) as usize;
            spreads.get(i).copied()
        };
        let received = self.frames_received();
        let grouped = self.frames_grouped();
        let mut drops = [0u64; DropReason::COUNT];
        for c in &self.cameras {
            for (total, d) in drops.iter_mut().zip(c.drops) {
                *total += d;
            }
        }
        SyncReport {
            groups_complete: self.groups_complete,
            groups_partial: self.groups_partial,
            groups_stale: self.groups_stale,
            frames_received: received,
            frames_grouped: grouped,
            match_rate: (received > 0).then(|| grouped as f64 / received as f64),
            spread_last_ns: self.spread_last,
            spread_p50_ns: quantile(0.5),
            spread_p99_ns: quantile(0.99),
            spread_max_ns: spreads.last().copied(),
            spread_max_ever_ns: self.spread_max_ever,
            drops,
            cameras: self
                .cameras
                .iter()
                .enumerate()
                .map(|(camera, c)| CameraSyncReport {
                    camera,
                    connected: connected(camera),
                    received: c.received,
                    grouped: c.grouped,
                    drops: c.drops,
                    period_ns: (c.period_ns > 0).then_some(c.period_ns),
                    offset_ns: c.offset_ns,
                    offset_mean_ns: c.drift.mean_offset_ns(),
                    drift_ppm: c.drift.ppm(),
                })
                .collect(),
        }
    }
}

/// A grouper's sync quality ([`Grouper::report`](super::Grouper::report)).
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SyncReport {
    /// Groups with every registered camera.
    pub groups_complete: u64,
    /// Groups missing a camera (`Partial`, or `Latest` while a camera is disconnected).
    pub groups_partial: u64,
    /// Groups replaced before they were taken.
    pub groups_stale: u64,
    pub frames_received: u64,
    /// Frames in groups handed out (or ready to be).
    pub frames_grouped: u64,
    /// `frames_grouped / frames_received` (`None` before the first frame). 1 when every frame
    /// found its partners; 0.5 for a 60 fps camera paired with a 30 fps one at
    /// [`RateMatch::Slowest`](super::RateMatch::Slowest).
    pub match_rate: Option<f64>,
    /// Latest minus earliest timestamp in the last group.
    pub spread_last_ns: Option<u64>,
    /// Spread percentiles over the last [`SPREAD_WINDOW`] groups.
    pub spread_p50_ns: Option<u64>,
    pub spread_p99_ns: Option<u64>,
    /// Largest spread in the window.
    pub spread_max_ns: Option<u64>,
    /// Largest spread since the start.
    pub spread_max_ever_ns: u64,
    /// Frames dropped, by [`DropReason::index`].
    pub drops: [u64; DropReason::COUNT],
    pub cameras: Vec<CameraSyncReport>,
}

impl SyncReport {
    /// Frames dropped for `reason`.
    pub fn drops_of(&self, reason: DropReason) -> u64 {
        self.drops[reason.index()]
    }

    /// Frames dropped for any reason.
    pub fn drops_total(&self) -> u64 {
        self.drops.iter().sum()
    }
}

/// One camera in a [`SyncReport`].
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CameraSyncReport {
    pub camera: CameraIndex,
    pub connected: bool,
    pub received: u64,
    pub grouped: u64,
    /// By [`DropReason::index`].
    pub drops: [u64; DropReason::COUNT],
    /// Frame period measured from its timestamps.
    pub period_ns: Option<u64>,
    /// Its timestamp minus the reference camera's, in the last group with both.
    pub offset_ns: Option<i64>,
    /// The same averaged over the drift window.
    pub offset_mean_ns: Option<f64>,
    /// How fast the offset changes: its clock's drift against the reference camera's, in ppm
    /// (µs per second). Free-running sensors on separate crystals drift by a few to tens of ppm;
    /// cameras sharing a clock, or hardware-synchronized, by ~0.
    pub drift_ppm: Option<f64>,
}

impl CameraSyncReport {
    pub fn drops_of(&self, reason: DropReason) -> u64 {
        self.drops[reason.index()]
    }
}
