//! Frames of several cameras grouped by sensor timestamp (docs/multi-camera-sync.md).
//!
//! A [`FrameGrouper`] takes frames from N cameras ([`GroupSource`]: in-process
//! [`CaptureHandle`](crate::capture_api::CaptureHandle)s and
//! [`MediaPipeline`](crate::session::MediaPipeline)s, camera service
//! [`FrameClient`](crate::ipc::FrameClient)s, any [`BoundedRx`] of frames), puts their
//! timestamps on one clock ([`ClockMode`]) and hands out [`FrameGroup`]s: one frame per camera,
//! taken within [`GroupConfig::tolerance_ns`] of each other. The grouping itself is
//! `styx_core::multicam` (`no_std`); this adds the sources, the clocks, the waiting and the
//! metrics.
//!
//! - **Policies** ([`GroupPolicy`]): `Strict` (every camera or nothing), `Partial` (what came by
//!   the deadline), `Latest` (only the newest complete group).
//! - **Bounded**: at most [`GroupConfig::depth`] frames per camera and
//!   [`GroupConfig::output_depth`] groups are held; older frames go back to their capture pool
//!   at once (counted by [`DropReason`]).
//! - **Waiting** like a [`FrameClient`](crate::ipc::FrameClient): [`FrameGrouper::try_next`]
//!   never blocks and the grouper's descriptor ([`AsFd`]) is readable when it has something to
//!   do; [`FrameGrouper::recv`] blocks up to a timeout; [`FrameGrouper::next`] and
//!   [`FrameGrouper::stream`] await on any executor.
//! - **Cameras come and go**: a reconnecting `FrameClient`'s `Disconnected` stops the grouper
//!   waiting for it (`Strict` waits for its return), its `Connected` brings it back;
//!   [`FrameGrouper::try_event`] reports them.
//! - **Sync quality**: [`FrameGrouper::report`] (spread p50/p99, offsets and drift per camera,
//!   match rate, drops by reason), and every grouper is in `styx::metrics::snapshot()` as
//!   `styx_sync_*`.
//!
//! ```no_run
//! use std::time::Duration;
//! use styx::multicam::{FrameGrouper, GroupConfig, GroupPolicy};
//! use styx::prelude::*;
//!
//! fn stereo(left: CaptureHandle, right: CaptureHandle) -> std::io::Result<()> {
//!     // Two 30 fps cameras: within half a frame (16 ms), both or nothing.
//!     let config = GroupConfig::new(16_000_000).policy(GroupPolicy::Strict);
//!     let mut grouper = FrameGrouper::new(config)?.named("stereo");
//!     let l = grouper.add("left", left)?;
//!     let r = grouper.add("right", right)?;
//!     while let RecvOutcome::Data(group) = grouper.recv(Duration::from_secs(1)) {
//!         let (Some(_left), Some(_right)) = (group.get(l), group.get(r)) else { continue };
//!         let _spread_us = group.spread_ns / 1000;
//!     }
//!     Ok(())
//! }
//! ```

mod clock;
mod poller;
mod source;

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use parking_lot::Mutex;
use smallvec::SmallVec;
use styx_core::multicam::Grouper;
use styx_core::prelude::*;

use crate::metrics::{SyncGroupMetrics, SyncSource};

pub use clock::{ClockError, ClockMode};
pub use source::{GroupSource, SourceEvent};
pub use styx_core::multicam::{
    CameraIndex, CameraSyncReport, DropReason, FrameGroup, GroupConfig, GroupPolicy, Member,
    RateMatch, SyncReport,
};

/// Events taken from one source per turn before the others get theirs.
const EVENTS_PER_SOURCE: usize = 16;
/// Camera changes kept unread (the oldest go first): bounded for callers using `try_next`.
const MAX_CHANGES: usize = 32;

/// What [`FrameGrouper::try_event`] returns.
// Moved once from the grouper to the caller: boxing the group would allocate per group.
#[allow(clippy::large_enum_variant)]
pub enum GroupEvent {
    /// Frames taken at the same time.
    Group(FrameGroup<FrameLease>),
    /// A camera (re)connected: groups need it again.
    Connected(CameraIndex),
    /// A camera's connection is gone (or its source closed): its pending frames went back.
    Disconnected(CameraIndex),
}

impl std::fmt::Debug for GroupEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Group(g) => write!(
                f,
                "Group(#{} cameras {:?} spread {} ns)",
                g.sequence,
                g.cameras().collect::<Vec<_>>(),
                g.spread_ns
            ),
            Self::Connected(camera) => write!(f, "Connected({camera})"),
            Self::Disconnected(camera) => write!(f, "Disconnected({camera})"),
        }
    }
}

/// What the metrics registry reads.
struct Shared {
    name: Mutex<String>,
    grouper: Mutex<Grouper<FrameLease>>,
    cameras: Mutex<Vec<String>>,
}

impl SyncSource for Shared {
    fn sync_metrics(&self) -> SyncGroupMetrics {
        let grouper = self.grouper.lock();
        SyncGroupMetrics {
            name: self.name.lock().clone(),
            policy: grouper.config().policy.as_str().into(),
            tolerance_ns: grouper.config().tolerance_ns,
            cameras: self.cameras.lock().clone(),
            report: grouper.report(),
        }
    }
}

struct Slot {
    name: String,
    source: Box<dyn GroupSource>,
    closed: bool,
}

/// Groups frames of several cameras by timestamp; see the [module docs](self).
pub struct FrameGrouper {
    shared: Arc<Shared>,
    slots: Vec<Option<Slot>>,
    clocks: clock::ClockMap,
    poller: poller::Poller,
    changes: VecDeque<GroupEvent>,
    last_error: Option<ClockError>,
}

impl FrameGrouper {
    /// A grouper without cameras, comparing timestamps on `CLOCK_MONOTONIC`. Fails only when
    /// its descriptors cannot be created.
    pub fn new(config: GroupConfig) -> io::Result<Self> {
        Ok(Self {
            shared: Arc::new(Shared {
                name: Mutex::new(String::new()),
                grouper: Mutex::new(Grouper::new(config)),
                cameras: Mutex::new(Vec::new()),
            }),
            slots: Vec::new(),
            clocks: clock::ClockMap::new(ClockMode::default()),
            poller: poller::Poller::new()?,
            changes: VecDeque::new(),
            last_error: None,
        })
    }

    /// Lists the grouper in `styx::metrics::snapshot()` under `name` (the `group` label of the
    /// `styx_sync_*` metrics) while it lives.
    pub fn named(self, name: impl Into<String>) -> Self {
        let mut current = self.shared.name.lock();
        let first = current.is_empty();
        *current = name.into();
        drop(current);
        if first {
            let weak: std::sync::Weak<dyn SyncSource> = Arc::downgrade(&self.shared) as _;
            crate::metrics::register_sync(weak);
        }
        self
    }

    /// Which clock timestamps are compared on (call before adding cameras).
    pub fn clock(mut self, mode: ClockMode) -> Self {
        self.clocks = clock::ClockMap::new(mode);
        self
    }

    pub fn clock_mode(&self) -> ClockMode {
        self.clocks.mode()
    }

    pub fn config(&self) -> GroupConfig {
        self.shared.grouper.lock().config().clone()
    }

    /// Adds a camera; groups need it from now on. Its index is its place in groups.
    pub fn add(
        &mut self,
        name: impl Into<String>,
        source: impl GroupSource + 'static,
    ) -> io::Result<CameraIndex> {
        let source: Box<dyn GroupSource> = Box::new(source);
        if let Some(fd) = source.fd() {
            self.poller.watch(fd)?;
        }
        let name = name.into();
        let index = self.shared.grouper.lock().add_camera();
        self.shared.cameras.lock().push(name.clone());
        self.slots.push(Some(Slot {
            name,
            source,
            closed: false,
        }));
        // Look at it on the next turn.
        self.poller.notify();
        Ok(index)
    }

    /// Removes a camera for good (groups stop needing it) and gives its source back.
    pub fn remove(&mut self, camera: CameraIndex) -> Option<Box<dyn GroupSource>> {
        let slot = self.slots.get_mut(camera)?.take()?;
        if let Some(fd) = slot.source.fd() {
            self.poller.unwatch(fd);
        }
        self.shared.grouper.lock().remove_camera(camera);
        self.poller.notify();
        Some(slot.source)
    }

    /// `camera`'s name.
    pub fn camera_name(&self, camera: CameraIndex) -> Option<&str> {
        self.slots.get(camera)?.as_ref().map(|s| s.name.as_str())
    }

    /// Cameras added (removed ones included: indices are not reused).
    pub fn cameras(&self) -> usize {
        self.slots.len()
    }

    pub fn is_connected(&self, camera: CameraIndex) -> bool {
        self.shared.grouper.lock().is_connected(camera)
    }

    /// The latest frame whose timestamp could not be put on the grouper's clock.
    pub fn last_error(&self) -> Option<ClockError> {
        self.last_error.clone()
    }

    /// Sync quality now.
    pub fn report(&self) -> SyncReport {
        self.shared.grouper.lock().report()
    }

    /// [`Self::report`] with the grouper's and cameras' names, as the metrics list it.
    pub fn metrics(&self) -> SyncGroupMetrics {
        self.shared.sync_metrics()
    }

    /// Frames held now: pending and in groups not taken (bounded by
    /// `cameras × (depth + output_depth)`).
    pub fn held(&self) -> usize {
        self.shared.grouper.lock().held()
    }

    /// Drops every pending frame and ready group (their buffers go back at once).
    pub fn clear(&mut self) {
        self.shared.grouper.lock().clear();
    }

    /// One turn: take what the sources have, form groups, arm the deadline timer. A source
    /// that disconnects or closes is read no further this turn, and is marked gone only after
    /// the groups its frames already complete have formed.
    fn pump(&mut self) {
        self.poller.clear();
        let mut grouper = self.shared.grouper.lock();
        let mut gone: SmallVec<[CameraIndex; 4]> = SmallVec::new();
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let Some(slot) = slot.as_mut().filter(|s| !s.closed) else {
                continue;
            };
            for taken in 0.. {
                if taken == EVENTS_PER_SOURCE {
                    // More waits: come back after the others.
                    self.poller.notify();
                    break;
                }
                match slot.source.poll_event(self.poller.waker()) {
                    SourceEvent::Frame(frame) => {
                        let arrived = CaptureInstant::now().as_nanos();
                        match self.clocks.map(&slot.name, frame.meta()) {
                            Ok(ts) => {
                                grouper.push(index, ts, arrived, frame);
                            }
                            Err(err) => {
                                drop(frame);
                                crate::trace::debug!(error = %err, "frame not grouped");
                                grouper.count_drop(index, DropReason::Clock);
                                self.last_error = Some(err);
                            }
                        }
                    }
                    SourceEvent::Connected => {
                        grouper.set_connected(index, true);
                        push_change(&mut self.changes, GroupEvent::Connected(index));
                    }
                    SourceEvent::Disconnected => {
                        gone.push(index);
                        // What follows belongs to the next connection: next turn.
                        self.poller.notify();
                        break;
                    }
                    SourceEvent::Empty => break,
                    SourceEvent::Closed => {
                        slot.closed = true;
                        gone.push(index);
                        break;
                    }
                }
            }
        }
        let now = CaptureInstant::now().as_nanos();
        grouper.poll(now);
        if !gone.is_empty() {
            for &index in &gone {
                grouper.set_connected(index, false);
                push_change(&mut self.changes, GroupEvent::Disconnected(index));
            }
            grouper.poll(now);
        }
        let after = grouper
            .next_wake()
            .map(|at| Duration::from_nanos(at.saturating_sub(now)));
        self.poller.wake_after(after);
    }

    fn all_closed(&self) -> bool {
        !self.slots.is_empty()
            && self
                .slots
                .iter()
                .all(|s| s.as_ref().is_none_or(|s| s.closed))
    }

    /// The next group or camera change without waiting: `Empty` when there is none (wait for
    /// the grouper's descriptor, [`AsFd`]), `Closed` once every source closed and every group
    /// was taken.
    pub fn try_event(&mut self) -> RecvOutcome<GroupEvent> {
        self.pump();
        if let Some(change) = self.changes.pop_front() {
            if !self.changes.is_empty() {
                self.poller.notify();
            }
            return RecvOutcome::Data(change);
        }
        let mut grouper = self.shared.grouper.lock();
        match grouper.pop() {
            Some(group) => {
                if grouper.ready() > 0 {
                    self.poller.notify();
                }
                RecvOutcome::Data(GroupEvent::Group(group))
            }
            None if self.all_closed() => RecvOutcome::Closed,
            None => RecvOutcome::Empty,
        }
    }

    /// The next group without waiting (camera changes are kept for [`Self::try_event`], the
    /// latest few).
    pub fn try_next(&mut self) -> RecvOutcome<FrameGroup<FrameLease>> {
        self.pump();
        let mut grouper = self.shared.grouper.lock();
        match grouper.pop() {
            Some(group) => {
                if grouper.ready() > 0 {
                    self.poller.notify();
                }
                RecvOutcome::Data(group)
            }
            None if self.all_closed() => RecvOutcome::Closed,
            None => RecvOutcome::Empty,
        }
    }

    /// Waits up to `wait` for a group: `Empty` when none formed in time.
    pub fn recv(&mut self, wait: Duration) -> RecvOutcome<FrameGroup<FrameLease>> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            match self.try_next() {
                RecvOutcome::Empty => {}
                outcome => return outcome,
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return RecvOutcome::Empty;
            }
            self.poller.wait(left);
        }
    }

    /// Waits up to `wait` for a group or camera change.
    pub fn recv_event(&mut self, wait: Duration) -> RecvOutcome<GroupEvent> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            match self.try_event() {
                RecvOutcome::Empty => {}
                outcome => return outcome,
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return RecvOutcome::Empty;
            }
            self.poller.wait(left);
        }
    }

    fn poll_with<T>(
        &mut self,
        cx: &mut Context<'_>,
        mut next: impl FnMut(&mut Self) -> RecvOutcome<T>,
    ) -> Poll<RecvOutcome<T>> {
        loop {
            match next(self) {
                RecvOutcome::Empty => {}
                outcome => return Poll::Ready(outcome),
            }
            match self.poller.reactor().map(|r| r.poll_read_ready(cx)) {
                Ok(Poll::Pending) => return Poll::Pending,
                Ok(Poll::Ready(Ok(_))) => {}
                Ok(Poll::Ready(Err(_))) | Err(_) => return Poll::Ready(RecvOutcome::Closed),
            }
        }
    }

    /// Poll for the next group from a hand-written future or stream: `Pending` with `cx`'s
    /// waker woken when the grouper's descriptor becomes readable (styx-graph's reactor: any
    /// executor).
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<RecvOutcome<FrameGroup<FrameLease>>> {
        self.poll_with(cx, Self::try_next)
    }

    /// [`Self::poll_next`] for groups and camera changes.
    pub fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<RecvOutcome<GroupEvent>> {
        self.poll_with(cx, Self::try_event)
    }

    /// The next group, awaited on any executor (as `FrameClient::next`).
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> NextGroup<'_> {
        NextGroup { grouper: self }
    }

    /// Groups as a [`Stream`](futures_core::Stream), ending when every source closed.
    pub fn stream(&mut self) -> GroupStream<'_> {
        GroupStream {
            grouper: self,
            done: false,
        }
    }
}

fn push_change(changes: &mut VecDeque<GroupEvent>, change: GroupEvent) {
    if changes.len() >= MAX_CHANGES {
        changes.pop_front();
    }
    changes.push_back(change);
}

/// The grouper's descriptor: readable when [`FrameGrouper::try_event`] has something to do (a
/// source has a frame or news, a group deadline passed). Wait on it with `poll`/`epoll`, then
/// call [`FrameGrouper::try_next`] (or `try_event`) until it returns `Empty`.
impl AsFd for FrameGrouper {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.poller.fd()
    }
}

impl AsRawFd for FrameGrouper {
    fn as_raw_fd(&self) -> RawFd {
        self.poller.fd().as_raw_fd()
    }
}

/// [`FrameGrouper::next`].
#[must_use = "futures do nothing unless polled"]
pub struct NextGroup<'a> {
    grouper: &'a mut FrameGrouper,
}

impl Future for NextGroup<'_> {
    type Output = RecvOutcome<FrameGroup<FrameLease>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.grouper.poll_next(cx)
    }
}

/// [`FrameGrouper::stream`].
#[must_use = "streams do nothing unless polled"]
pub struct GroupStream<'a> {
    grouper: &'a mut FrameGrouper,
    done: bool,
}

impl futures_core::Stream for GroupStream<'_> {
    type Item = FrameGroup<FrameLease>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        match self.grouper.poll_next(cx) {
            Poll::Ready(RecvOutcome::Data(group)) => Poll::Ready(Some(group)),
            Poll::Ready(_) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests;
