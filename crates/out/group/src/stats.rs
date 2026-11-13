//! Counters behind a grouper: frames per camera, drops per reason, groups, spread. Every field
//! is a fixed-size array, so recording a frame or a group never allocates.

use crate::{CameraIndex, DropReason};

/// Groups whose spread is kept for the percentiles.
pub const SPREAD_WINDOW: usize = 256;

/// Counters of a [`Grouper`](crate::Grouper) with `N` camera slots.
#[derive(Clone, Debug)]
pub struct Stats<const N: usize> {
    received: [u64; N],
    grouped: [u64; N],
    drops: [[u64; DropReason::COUNT]; N],
    groups: u64,
    groups_complete: u64,
    groups_partial: u64,
    groups_stale: u64,
    spreads: [u64; SPREAD_WINDOW],
    spread_len: usize,
    spread_next: usize,
    spread_last_ns: u64,
    spread_max_ns: u64,
}

impl<const N: usize> Stats<N> {
    pub(crate) const fn new() -> Self {
        Self {
            received: [0; N],
            grouped: [0; N],
            drops: [[0; DropReason::COUNT]; N],
            groups: 0,
            groups_complete: 0,
            groups_partial: 0,
            groups_stale: 0,
            spreads: [0; SPREAD_WINDOW],
            spread_len: 0,
            spread_next: 0,
            spread_last_ns: 0,
            spread_max_ns: 0,
        }
    }

    pub(crate) fn add_received(&mut self, camera: CameraIndex) {
        if let Some(n) = self.received.get_mut(camera) {
            *n += 1;
        }
    }

    pub(crate) fn add_grouped(&mut self, camera: CameraIndex) {
        if let Some(n) = self.grouped.get_mut(camera) {
            *n += 1;
        }
    }

    /// A grouped frame whose group went stale: it is counted as a drop instead.
    pub(crate) fn remove_grouped(&mut self, camera: CameraIndex) {
        if let Some(n) = self.grouped.get_mut(camera) {
            *n = n.saturating_sub(1);
        }
    }

    pub(crate) fn add_drop(&mut self, camera: CameraIndex, reason: DropReason) {
        if let Some(row) = self.drops.get_mut(camera) {
            row[reason.index()] += 1;
        }
    }

    pub(crate) fn add_group(&mut self, spread_ns: u64, complete: bool) {
        self.groups += 1;
        if complete {
            self.groups_complete += 1;
        } else {
            self.groups_partial += 1;
        }
        self.spreads[self.spread_next] = spread_ns;
        self.spread_next = (self.spread_next + 1) % SPREAD_WINDOW;
        self.spread_len = (self.spread_len + 1).min(SPREAD_WINDOW);
        self.spread_last_ns = spread_ns;
        self.spread_max_ns = self.spread_max_ns.max(spread_ns);
    }

    pub(crate) fn add_stale_group(&mut self) {
        self.groups_stale += 1;
    }

    /// Frames received by `camera` (including dropped ones).
    pub fn received_by(&self, camera: CameraIndex) -> u64 {
        self.received.get(camera).copied().unwrap_or(0)
    }

    /// Frames of `camera` that went into a group handed out or still held in one.
    pub fn grouped_by(&self, camera: CameraIndex) -> u64 {
        self.grouped.get(camera).copied().unwrap_or(0)
    }

    /// Frames of `camera` dropped for `reason`.
    pub fn dropped_by(&self, camera: CameraIndex, reason: DropReason) -> u64 {
        self.drops.get(camera).map_or(0, |row| row[reason.index()])
    }

    /// Frames dropped for `reason`, over every camera.
    pub fn dropped(&self, reason: DropReason) -> u64 {
        self.drops.iter().map(|row| row[reason.index()]).sum()
    }

    /// Every drop, over every camera and reason.
    pub fn drops_total(&self) -> u64 {
        self.drops.iter().flatten().sum()
    }

    /// Frames received over every camera.
    pub fn frames_received(&self) -> u64 {
        self.received.iter().sum()
    }

    /// Frames over every camera that went into a group.
    pub fn frames_grouped(&self) -> u64 {
        self.grouped.iter().sum()
    }

    /// Frames grouped ÷ frames received (`None` before any frame).
    pub fn match_rate(&self) -> Option<f64> {
        let received = self.frames_received();
        (received > 0).then(|| self.frames_grouped() as f64 / received as f64)
    }

    /// Groups emitted (complete or partial).
    pub fn groups(&self) -> u64 {
        self.groups
    }

    /// Groups emitted with every registered camera in them.
    pub fn groups_complete(&self) -> u64 {
        self.groups_complete
    }

    /// Groups emitted short of a camera (`Partial` and `Latest` only).
    pub fn groups_partial(&self) -> u64 {
        self.groups_partial
    }

    /// Groups replaced or dropped before anyone took them.
    pub fn groups_stale(&self) -> u64 {
        self.groups_stale
    }

    /// Spread of the latest emitted group, in nanoseconds.
    pub fn spread_last_ns(&self) -> Option<u64> {
        (self.groups > 0).then_some(self.spread_last_ns)
    }

    /// Largest spread emitted, in nanoseconds.
    pub fn spread_max_ns(&self) -> Option<u64> {
        (self.groups > 0).then_some(self.spread_max_ns)
    }

    /// Spread percentile over the last [`SPREAD_WINDOW`] groups, `percent` in `0..=100`
    /// (nearest rank, rounded down).
    pub fn spread_percentile_ns(&self, percent: u8) -> Option<u64> {
        if self.spread_len == 0 {
            return None;
        }
        let mut window = [0u64; SPREAD_WINDOW];
        window[..self.spread_len].copy_from_slice(&self.spreads[..self.spread_len]);
        let sorted = &mut window[..self.spread_len];
        sorted.sort_unstable();
        let rank = (self.spread_len - 1) * usize::from(percent.min(100)) / 100;
        Some(sorted[rank])
    }
}
