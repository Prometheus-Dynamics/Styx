//! A running shared capture: consumers join and leave it while it runs.
//!
//! Frames are pulled, not pushed: the consumer that asks first reads the capture and leaves a
//! zero-copy share of the frame for every other group of consumers. A group prepares frames once
//! (decode, scale, pyramid) the same way; its members share the result, each cropping its own
//! region. Nothing reads the camera while nobody asks, so an idle capture can stop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_codec::Codec;
use styx_core::prelude::*;
use styx_core::queue::{BoundedRx, BoundedTx, QueueOverflow, RecvWaitOutcome, bounded_with};

use super::FramePlan;
use super::start::{FramePreparer, RoiHandle};
use crate::capture_api::CaptureHandle;

/// Pull-based fan-out of frames to receivers that come and go: the first to ask reads the source
/// and leaves a zero-copy share of each frame for the others.
pub(crate) struct Fanout {
    branches: Mutex<Vec<Option<Receiver>>>,
    /// Set while a receiver reads the source; the others wait on their own queues.
    pulling: AtomicBool,
}

/// One receiver of a [`Fanout`]: its queue, and how often it takes frames.
struct Receiver {
    tx: BoundedTx<FrameLease>,
    /// At least this long between its frames (`FrameRequest::deliver_at_most`): it gets no
    /// share of the frames in between, so they are neither prepared nor held for it.
    min_interval: Option<Duration>,
    /// When its next frame is due.
    next: Option<Instant>,
    /// Due the frame being fanned out ([`Fanout::fan_out`]).
    pending: bool,
}

impl Receiver {
    /// Whether it takes a frame now (and if so, counts it as taken). Paced like the preview's
    /// gate: a frame up to an eighth of the interval early still counts, and each next one is
    /// due an interval after the last was due, so frames on a camera's grid keep the rate.
    fn due(&mut self, now: Instant) -> bool {
        let Some(interval) = self.min_interval else {
            return true;
        };
        match self.next {
            Some(next) if now + interval / 8 < next => false,
            Some(next) if now < next + interval => {
                self.next = Some(next + interval);
                true
            }
            _ => {
                self.next = Some(now + interval);
                true
            }
        }
    }
}

/// Held by the receiver reading the source.
struct Pulling<'a>(&'a AtomicBool);

impl Drop for Pulling<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Fanout {
    pub(crate) fn new() -> Self {
        Self {
            branches: Mutex::new(Vec::new()),
            pulling: AtomicBool::new(false),
        }
    }

    /// A new receiver taking at most one frame per `min_interval` (none: every frame); returns
    /// its index.
    pub(crate) fn add_limited(
        &self,
        tx: BoundedTx<FrameLease>,
        min_interval: Option<Duration>,
    ) -> usize {
        let tx = Receiver {
            tx,
            min_interval,
            next: None,
            pending: false,
        };
        let mut branches = self.branches.lock();
        match branches.iter().position(Option::is_none) {
            Some(free) => {
                branches[free] = Some(tx);
                free
            }
            None => {
                branches.push(Some(tx));
                branches.len() - 1
            }
        }
    }

    pub(crate) fn remove(&self, index: usize) {
        if let Some(slot) = self.branches.lock().get_mut(index) {
            *slot = None;
        }
    }

    fn try_pull(&self) -> Option<Pulling<'_>> {
        self.pulling
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Pulling(&self.pulling))
    }

    /// Next frame for receiver `index` (receiving on `rx`), waiting up to `wait`. `source(wait)`
    /// reads the next frame from the source, waiting up to `wait` (zero: without waiting).
    pub(crate) fn next(
        &self,
        index: usize,
        rx: &BoundedRx<FrameLease>,
        wait: Duration,
        mut source: impl FnMut(Duration) -> RecvOutcome<FrameLease>,
    ) -> RecvOutcome<FrameLease> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(_pulling) = self.try_pull() {
                // A frame the source already holds is newer than any share queued for us:
                // hand it to every receiver first so none serves a stale frame.
                self.take_ready(source(Duration::ZERO));
            }
            match rx.recv() {
                RecvOutcome::Empty => {}
                other => return other,
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return RecvOutcome::Empty;
            }
            if let Some(_pulling) = self.try_pull() {
                match source(remaining.min(Duration::from_millis(50))) {
                    RecvOutcome::Data(frame) => {
                        if let Some(frame) = self.fan_out(frame, Some(index)) {
                            return RecvOutcome::Data(frame);
                        }
                    }
                    RecvOutcome::Empty => {}
                    RecvOutcome::Closed => {
                        self.close();
                        return rx.recv();
                    }
                }
            } else {
                // Another receiver is reading the source and will leave us a share.
                match rx.recv_timeout(remaining.min(Duration::from_millis(5))) {
                    RecvWaitOutcome::Data(frame) => return RecvOutcome::Data(frame),
                    RecvWaitOutcome::Closed => return RecvOutcome::Closed,
                    RecvWaitOutcome::Timeout => {}
                }
            }
        }
    }

    /// [`Fanout::next`] without a deadline, awaiting the source: `now()` reads a frame the
    /// source already holds, `wait()` awaits the next one.
    #[cfg(feature = "async")]
    pub(crate) async fn next_async<Fut>(
        &self,
        index: usize,
        rx: &BoundedRx<FrameLease>,
        now: impl Fn() -> RecvOutcome<FrameLease>,
        wait: impl Fn() -> Fut,
    ) -> RecvOutcome<FrameLease>
    where
        Fut: std::future::Future<Output = RecvOutcome<FrameLease>>,
    {
        loop {
            if let Some(_pulling) = self.try_pull() {
                self.take_ready(now());
            }
            match rx.recv() {
                RecvOutcome::Empty => {}
                other => return other,
            }
            if let Some(_pulling) = self.try_pull() {
                match wait().await {
                    RecvOutcome::Data(frame) => {
                        if let Some(frame) = self.fan_out(frame, Some(index)) {
                            return RecvOutcome::Data(frame);
                        }
                    }
                    RecvOutcome::Empty => {}
                    RecvOutcome::Closed => {
                        self.close();
                        return rx.recv();
                    }
                }
            } else if let Ok(outcome) =
                tokio::time::timeout(Duration::from_millis(5), rx.recv_async()).await
            {
                return outcome;
            }
        }
    }

    /// Hand a frame the source already had to every receiver.
    fn take_ready(&self, outcome: RecvOutcome<FrameLease>) {
        match outcome {
            RecvOutcome::Data(frame) => drop(self.fan_out(frame, None)),
            RecvOutcome::Closed => self.close(),
            RecvOutcome::Empty => {}
        }
    }

    /// Send shares of `frame` to every receiver due a frame but `keep`, returning the frame
    /// itself for `keep` when it is due one (receivers taking fewer frames skip the others).
    fn fan_out(&self, frame: FrameLease, keep: Option<usize>) -> Option<FrameLease> {
        let now = Instant::now();
        let mut branches = self.branches.lock();
        let mut kept = keep.is_none();
        let mut others = 0usize;
        for (i, receiver) in branches.iter_mut().enumerate() {
            let Some(r) = receiver else { continue };
            let due = r.due(now);
            if Some(i) == keep {
                kept = due;
                r.pending = false;
            } else {
                r.pending = due;
                others += usize::from(due);
            }
        }
        if others > 0 {
            let frame = frame.into_shareable();
            for r in branches.iter_mut().flatten() {
                if std::mem::take(&mut r.pending)
                    && let Some(share) = frame.share()
                {
                    r.tx.send(share);
                }
            }
            return kept.then_some(frame);
        }
        kept.then_some(frame)
    }

    fn close(&self) {
        self.branches
            .lock()
            .iter()
            .flatten()
            .for_each(|r| r.tx.close());
    }
}

/// The capture behind a shared plan; stopped when nothing uses it any more.
pub(crate) struct SharedCapture {
    capture: Option<CaptureHandle>,
    /// One receiver per group of consumers.
    fanout: Fanout,
}

impl SharedCapture {
    pub(crate) fn new(capture: CaptureHandle) -> Arc<Self> {
        Arc::new(Self {
            capture: Some(capture),
            fanout: Fanout::new(),
        })
    }

    pub(crate) fn capture(&self) -> &CaptureHandle {
        self.capture.as_ref().expect("capture runs until dropped")
    }

    /// Next captured frame for group `index` (receiving on `rx`), waiting up to `wait`.
    fn next(
        &self,
        index: usize,
        rx: &BoundedRx<FrameLease>,
        wait: Duration,
    ) -> RecvOutcome<FrameLease> {
        let capture = self.capture();
        self.fanout.next(index, rx, wait, |wait| {
            if wait.is_zero() {
                capture.recv()
            } else {
                capture.recv_blocking(wait)
            }
        })
    }

    #[cfg(feature = "async")]
    async fn next_async(
        &self,
        index: usize,
        rx: &BoundedRx<FrameLease>,
    ) -> RecvOutcome<FrameLease> {
        let capture = self.capture();
        self.fanout
            .next_async(index, rx, || capture.recv(), || capture.recv_async())
            .await
    }
}

impl Drop for SharedCapture {
    fn drop(&mut self) {
        if let Some(capture) = self.capture.take() {
            capture.stop();
        }
    }
}

/// Consumers whose frames are prepared the same way (see [`same_preparation`]): each captured
/// frame is prepared once, by whichever member asks first, and shared with the others.
pub(crate) struct PreparedGroup {
    shared: Arc<SharedCapture>,
    /// This group's receiver on the shared capture.
    index: usize,
    raw_rx: BoundedRx<FrameLease>,
    pub(crate) preparer: FramePreparer,
    fanout: Fanout,
    plan: FramePlan,
    /// Prepared without a region, so members can join (each crops its own region).
    open: bool,
}

impl PreparedGroup {
    /// Next prepared frame for `member`, waiting up to `wait`.
    fn next(
        &self,
        member: usize,
        rx: &BoundedRx<FrameLease>,
        wait: Duration,
    ) -> RecvOutcome<FrameLease> {
        self.fanout.next(member, rx, wait, |wait| {
            self.prepared(self.shared.next(self.index, &self.raw_rx, wait))
        })
    }

    #[cfg(feature = "async")]
    async fn next_async(
        &self,
        member: usize,
        rx: &BoundedRx<FrameLease>,
    ) -> RecvOutcome<FrameLease> {
        self.fanout
            .next_async(
                member,
                rx,
                || self.prepared(self.shared.next(self.index, &self.raw_rx, Duration::ZERO)),
                || async { self.prepared(self.shared.next_async(self.index, &self.raw_rx).await) },
            )
            .await
    }

    /// A captured frame prepared for this group; a frame that fails to prepare (e.g. a corrupt
    /// JPEG, or one without this group's ISP region) is skipped.
    fn prepared(&self, outcome: RecvOutcome<FrameLease>) -> RecvOutcome<FrameLease> {
        match outcome {
            RecvOutcome::Data(frame) => {
                match super::region_frames::consumer_frame(frame, &self.plan)
                    .and_then(|frame| self.preparer.process(frame))
                {
                    Ok(frame) => RecvOutcome::Data(frame),
                    Err(err) => {
                        crate::trace::debug!(group = self.index, error = %err, "frame skipped");
                        RecvOutcome::Empty
                    }
                }
            }
            other => other,
        }
    }
}

impl Drop for PreparedGroup {
    fn drop(&mut self) {
        self.shared.fanout.remove(self.index);
        self.raw_rx.close();
    }
}

/// One consumer of a shared capture.
pub(crate) struct Branch {
    pub(crate) group: Arc<PreparedGroup>,
    member: usize,
    rx: BoundedRx<FrameLease>,
    /// In an open group: this consumer's region, cropped from the shared frames.
    roi: Option<RoiHandle>,
    /// The group encodes H.264/H.265: after a gap, packets are useless until a keyframe.
    inter_coded: bool,
    awaiting_keyframe: bool,
    /// Packets this consumer's queue dropped so far.
    seen_evictions: u64,
    /// This consumer in the capture's metrics.
    metrics: Arc<crate::metrics::ConsumerStats>,
}

impl Branch {
    pub(crate) fn next(&mut self, wait: Duration) -> RecvOutcome<FrameLease> {
        let outcome = self.group.next(self.member, &self.rx, wait);
        let outcome = self.decodable(self.cropped(outcome));
        self.metrics.count(outcome)
    }

    #[cfg(feature = "async")]
    pub(crate) async fn next_async(&mut self) -> RecvOutcome<FrameLease> {
        loop {
            let outcome = self.group.next_async(self.member, &self.rx).await;
            let outcome = self.decodable(self.cropped(outcome));
            match self.metrics.count(outcome) {
                RecvOutcome::Empty => {}
                other => return other,
            }
        }
    }

    pub(crate) fn request_keyframe(&self) {
        self.group.preparer.request_keyframe();
    }

    /// The shared capture.
    pub(crate) fn capture(&self) -> &CaptureHandle {
        self.group.shared.capture()
    }

    /// This consumer's frames and drops, as the capture's metrics list it.
    pub(crate) fn consumer_metrics(&self) -> crate::metrics::ConsumerMetrics {
        self.metrics.snapshot()
    }

    /// Skip inter-coded packets this consumer cannot decode: after joining a running stream, or
    /// after its queue dropped packets, until the keyframe it asks the encoder for.
    fn decodable(&mut self, outcome: RecvOutcome<FrameLease>) -> RecvOutcome<FrameLease> {
        if !self.inter_coded {
            return outcome;
        }
        let evictions = self.rx.stats().evictions;
        if evictions != self.seen_evictions {
            // Packets were lost, perhaps the keyframe asked for earlier: ask (again).
            self.seen_evictions = evictions;
            self.awaiting_keyframe = true;
            self.request_keyframe();
        }
        match outcome {
            RecvOutcome::Data(packet) if self.awaiting_keyframe && packet.meta().delta => {
                RecvOutcome::Empty
            }
            RecvOutcome::Data(packet) => {
                self.awaiting_keyframe = false;
                RecvOutcome::Data(packet)
            }
            other => other,
        }
    }

    fn cropped(&self, outcome: RecvOutcome<FrameLease>) -> RecvOutcome<FrameLease> {
        match (outcome, &self.roi) {
            (RecvOutcome::Data(frame), Some(roi)) => match self.group.preparer.crop(frame, roi) {
                Ok(frame) => RecvOutcome::Data(frame),
                Err(err) => {
                    crate::trace::warn!(consumer = self.member, error = %err, "frame skipped");
                    RecvOutcome::Empty
                }
            },
            (outcome, _) => outcome,
        }
    }

    pub(crate) fn health_report(&self) -> crate::metrics::HealthReport {
        let group = &self.group;
        let mut report = group.shared.capture().health_report();
        let evictions = self.rx.stats().evictions + group.raw_rx.stats().evictions;
        crate::metrics::push_drop_reason(
            &mut report.drop_reasons,
            crate::metrics::FrameDropReason::CaptureQueueEviction,
            evictions,
        );
        report.drop_count = crate::metrics::total_frame_drops(&report.drop_reasons);
        report
    }
}

impl Drop for Branch {
    fn drop(&mut self) {
        // Frames shared with a consumer that is gone would hold camera buffers.
        self.group.fanout.remove(self.member);
        self.rx.close();
    }
}

/// Whether two consumers' frames are prepared the same way: the same request apart from the
/// region of interest (applied per consumer, as a crop of the shared frame), the frame rate
/// (the capture has one) and how often it takes frames, on the same route.
pub(crate) fn same_preparation(a: &FramePlan, b: &FramePlan) -> bool {
    let key = |plan: &FramePlan| {
        let mut req = plan.request.clone();
        req.roi = None;
        req.extra_regions.fill(FrameRect::new(0, 0, 0, 0));
        req.fps = Default::default();
        req.min_interval = None;
        req
    };
    key(a) == key(b)
        && a.route.same_as(&b.route)
        && a.decode_scale == b.decode_scale
        && a.isp_output == b.isp_output
        && a.m2m_scale == b.m2m_scale
        && a.isp_format == b.isp_format
        && a.sensor_raw == b.sensor_raw
        && a.isp_second_output == b.isp_second_output
        && a.isp_pyramid_level == b.isp_pyramid_level
        // Views are cut per consumer; ISP regions come from the consumer's own outputs.
        && a.region.isp() == b.region.isp()
        && (!a.region.isp() || a.region.places == b.region.places)
        && a.region.overview == b.region.overview
        && a.exportable == b.exportable
}

/// Whether two plans give a consumer the same frames the same way: the same capture (backend,
/// mode, rate), the same preparation and ISP outputs ([`same_preparation`]), regions and queue.
/// A low-priority camera service client must leave every normal client's plan so.
pub(crate) fn same_plan(a: &FramePlan, b: &FramePlan) -> bool {
    a.backend == b.backend
        && a.mode.id == b.mode.id
        && a.interval == b.interval
        && a.queue_depth == b.queue_depth
        && a.request.roi == b.request.roi
        && a.region.places == b.region.places
        && same_preparation(a, b)
}

/// A running shared capture that consumers can join.
pub(crate) struct SharedSession {
    shared: Arc<SharedCapture>,
    groups: Mutex<Vec<Weak<PreparedGroup>>>,
}

impl SharedSession {
    pub(crate) fn new(capture: CaptureHandle) -> Self {
        Self {
            shared: SharedCapture::new(capture),
            groups: Mutex::new(Vec::new()),
        }
    }

    /// The camera capture every consumer shares (controls, descriptor).
    pub(crate) fn capture(&self) -> &CaptureHandle {
        self.shared.capture()
    }

    /// A consumer for `plan` (which must fit the running capture). With `share`, it joins an
    /// open group preparing frames the same way, or starts one; otherwise it gets its own group,
    /// which applies its region while preparing (e.g. a JPEG decode skips the rows below it).
    pub(crate) fn attach(&self, plan: &FramePlan, share: bool) -> super::Frames {
        // A raw consumer joining a running capture: its raw companions, without a restart.
        #[cfg(feature = "native")]
        if plan.sensor_raw {
            let on = ControlValue::Bool(true);
            if let Err(e) = self
                .shared
                .capture()
                .set_control(crate::capture_api::SENSOR_RAW, on)
            {
                crate::trace::warn!(error = %e, "raw frames for a joining consumer");
            }
        }
        let roi = RoiHandle::default();
        roi.set_regions(&plan.request.all_regions());
        if plan.region.isp() {
            roi.crop_in_isp(self.shared.capture(), plan.region.places.clone());
        }
        let depth = plan.queue_depth.max(1);
        let mut groups = self.groups.lock();
        groups.retain(|g| g.strong_count() > 0);
        let joined = share
            .then(|| {
                groups.iter().filter_map(Weak::upgrade).find(|g| {
                    g.open
                        && same_preparation(&g.plan, plan)
                        && (g.plan.request.min_interval.is_none()
                            || g.plan.request.min_interval == plan.request.min_interval)
                })
            })
            .flatten();
        let joined_running = joined.is_some();
        let group = joined.unwrap_or_else(|| {
            let (raw_tx, raw_rx) = bounded_with(depth, QueueOverflow::DropOldest);
            // A group takes frames as often as its request asks (`deliver_at_most`): members
            // join a group asking the same, or one taking every frame and skip their share.
            let index = self
                .shared
                .fanout
                .add_limited(raw_tx, plan.request.min_interval);
            let group = Arc::new(PreparedGroup {
                shared: self.shared.clone(),
                index,
                raw_rx,
                preparer: FramePreparer::new(
                    plan,
                    if share {
                        RoiHandle::default()
                    } else {
                        roi.clone()
                    },
                )
                .for_shared(),
                fanout: Fanout::new(),
                plan: plan.clone(),
                open: share,
            });
            groups.push(Arc::downgrade(&group));
            group
        });
        let (tx, rx) = bounded_with(depth, QueueOverflow::DropOldest);
        let member = group.fanout.add_limited(
            tx,
            plan.request
                .min_interval
                .filter(|_| group.plan.request.min_interval.is_none()),
        );
        let inter_coded = plan.inter_coded();
        if inter_coded && joined_running {
            // It joins a running stream: its first packets need a keyframe.
            group.preparer.request_keyframe();
        }
        let metrics = crate::metrics::ConsumerStats::for_queues(
            &self.shared.capture().live,
            format!("consumer {}.{member}", group.index),
            [rx.clone(), group.raw_rx.clone()],
        );
        let branch = Branch {
            metrics,
            roi: group.open.then(|| roi.clone()),
            group,
            member,
            rx,
            inter_coded,
            awaiting_keyframe: inter_coded,
            seen_evictions: 0,
        };
        super::Frames::branch(plan, branch, roi)
    }
}

#[cfg(test)]
mod pacing_tests {
    use super::*;

    fn receiver(fps: f64) -> Receiver {
        let (tx, _rx) = bounded_with(1, QueueOverflow::DropOldest);
        Receiver {
            tx,
            min_interval: Some(Duration::from_secs_f64(1.0 / fps)),
            next: None,
            pending: false,
        }
    }

    /// A 15 fps receiver of a 120 fps camera takes every eighth frame, jitter or not.
    #[test]
    fn a_limited_receiver_keeps_its_rate_on_the_cameras_grid() {
        let mut r = receiver(15.0);
        let start = Instant::now();
        let frame = Duration::from_secs_f64(1.0 / 120.0);
        let taken = (0..1200u32)
            .filter(|&i| {
                // +-0.4 ms of arrival jitter.
                let jitter = Duration::from_micros(u64::from(i % 9) * 100);
                r.due(start + frame * i + jitter - Duration::from_micros(400).min(jitter))
            })
            .count();
        assert!(
            (148..=152).contains(&taken),
            "{taken} of 1200 frames (10 s at 120 fps)"
        );
    }

    #[test]
    fn an_unlimited_receiver_takes_every_frame() {
        let (tx, _rx) = bounded_with(1, QueueOverflow::DropOldest);
        let mut r = Receiver {
            tx,
            min_interval: None,
            next: None,
            pending: false,
        };
        let now = Instant::now();
        assert!((0..10).all(|_| r.due(now)));
    }
}
