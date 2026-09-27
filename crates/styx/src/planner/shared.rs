//! Several consumers of one camera: one capture, and a branch per consumer that prepares its
//! own frames (decode, scale, ROI, pyramid).
//!
//! Frames are pulled, not pushed: the consumer that asks first reads the capture and leaves a
//! zero-copy share of the frame in every other branch's latest-frame queue. Each branch then
//! prepares only the frames its consumer takes, and nothing reads the camera while nobody asks
//! (so `StyxConfig::stop_when_idle` can stop it).

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_codec::CodecRegistryHandle;
use styx_core::prelude::*;
use styx_core::queue::{BoundedRx, BoundedTx, QueueOverflow, RecvWaitOutcome, bounded_with};

use super::routes::{self, Candidate, backend_name, describe};
use super::start::{PreparedGroup, RoiHandle};
use super::{
    FramePlan, PlanError, PlanRejection, RankKey, StepKind, cost, default_registry, plan_from,
};
use crate::BackendKind;
use crate::capture_api::{CaptureError, CaptureHandle, CaptureRequest, IdleStop, StyxConfig};
use crate::prelude::{Interval, Mode, ProbedBackend, ProbedDevice};

/// One capture serving several consumers, each with its own [`FramePlan`].
#[derive(Clone)]
pub struct SharedFramePlan {
    pub device: ProbedDevice,
    pub backend: BackendKind,
    pub mode: Mode,
    pub interval: Option<Interval>,
    /// One plan per consumer, in the order the requirements were given, all on this capture.
    pub consumers: Vec<FramePlan>,
    pub rejected: Vec<PlanRejection>,
    stop_when_idle: Option<(Duration, IdleStop)>,
    /// Consumers (indices) whose frames are prepared once and shared.
    groups: Vec<Vec<usize>>,
}

impl fmt::Debug for SharedFramePlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedFramePlan")
            .field("device", &self.device.identity.display)
            .field("mode", &self.mode.format)
            .field("consumers", &self.consumers)
            .finish()
    }
}

impl fmt::Display for SharedFramePlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let res = self.mode.format.resolution;
        writeln!(
            f,
            "shared capture of {} via {} {} {}x{} for {} consumers",
            self.device.identity.display,
            backend_name(self.backend),
            self.mode.format.code,
            res.width,
            res.height,
            self.consumers.len()
        )?;
        for (index, plan) in self.consumers.iter().enumerate() {
            write!(f, "consumer {index}: {plan}")?;
        }
        Ok(())
    }
}

/// Plan `requirements` (one per consumer) on one capture of `device`, using the default codec
/// registry.
pub fn plan_many(
    device: &ProbedDevice,
    requirements: &[FrameRequirements],
) -> Result<SharedFramePlan, PlanError> {
    plan_many_with(device, requirements, &default_registry()?)
}

/// [`plan_many`] with `registry` to find decoders.
pub fn plan_many_with(
    device: &ProbedDevice,
    requirements: &[FrameRequirements],
    registry: &CodecRegistryHandle,
) -> Result<SharedFramePlan, PlanError> {
    if requirements.is_empty() {
        return Err(PlanError::NoConsumers);
    }
    let mut rejected = Vec::new();
    let mut best: Option<(RankKey, &ProbedBackend, &Mode, Vec<Candidate<'_>>)> = None;
    for backend in &device.backends {
        // Every consumer's backend override must allow it.
        if requirements.iter().any(|req| {
            req.overrides
                .backend
                .as_ref()
                .is_some_and(|b| !b.eq_ignore_ascii_case(backend_name(backend.kind)))
        }) {
            continue;
        }
        for mode in &backend.descriptor.modes {
            let candidates: Result<Vec<_>, String> = requirements
                .iter()
                .enumerate()
                .map(|(i, req)| {
                    routes::candidate(backend, mode, req, registry)
                        .map_err(|reason| format!("consumer {i}: {reason}"))
                })
                .collect();
            match candidates {
                Ok(candidates) => {
                    let key = shared_rank(&candidates, requirements);
                    if best.as_ref().is_none_or(|(best_key, ..)| key < *best_key) {
                        best = Some((key, backend, mode, candidates));
                    }
                }
                Err(reason) => rejected.push(PlanRejection {
                    candidate: describe(backend, mode),
                    reason,
                }),
            }
        }
    }
    let Some((_, backend, mode, mut candidates)) = best else {
        return Err(PlanError::NoCandidates { rejected });
    };
    // The ISP has two outputs: the main one at the larger size consumers want and the second
    // at the smaller one. Consumers that fit neither (a third size, or with the second output
    // taken by an ISP pyramid) get the mode's size and scale on their own.
    let res = mode.format.resolution;
    let mode_size = (res.width.get(), res.height.get());
    let delivered = |c: &Candidate<'_>| c.isp_output.unwrap_or(mode_size);
    let mut sizes: Vec<(u32, u32)> = candidates.iter().map(delivered).collect();
    sizes.sort_by_key(|&(w, h)| std::cmp::Reverse(u64::from(w) * u64::from(h)));
    sizes.dedup();
    let two_outputs = sizes.len() == 2
        && routes::has_isp_second_output(backend)
        && candidates.iter().all(|c| c.isp_pyramid_level.is_none());
    let second_output = two_outputs.then(|| sizes[1]);
    if sizes.len() > 1 && !two_outputs {
        for (i, req) in requirements.iter().enumerate() {
            if candidates[i].isp_output.is_some() {
                let mut full = req.clone();
                full.output_resolution = None;
                let mut candidate =
                    routes::candidate(backend, mode, &full, registry).map_err(|reason| {
                        PlanError::NoCandidates {
                            rejected: vec![PlanRejection {
                                candidate: describe(backend, mode),
                                reason,
                            }],
                        }
                    })?;
                candidate.notes.push(
                    "other consumers of this capture need different sizes, so the ISP delivers \
                     the mode's size"
                        .into(),
                );
                candidates[i] = candidate;
            }
        }
    }
    let interval = shared_interval(mode, requirements);
    let consumers: Vec<FramePlan> = candidates
        .into_iter()
        .zip(requirements)
        .map(|(candidate, req)| {
            let second = second_output.is_some_and(|size| delivered(&candidate) == size);
            let mut plan = plan_from(device, candidate, req, interval, Vec::new());
            if second {
                plan.isp_second_output = true;
                if let Some(step) = plan.steps.iter_mut().find(|s| s.kind == StepKind::Scale) {
                    step.detail = format!("{}, on its second output", step.detail);
                }
            }
            plan
        })
        .collect();
    let mut consumers: Vec<FramePlan> = consumers;
    let groups = prepare_groups(&consumers);
    for group in groups.iter().filter(|g| g.len() > 1) {
        let list = group
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        for &i in group {
            consumers[i].notes.push(format!(
                "prepared once for consumers {list}, which share the frames"
            ));
        }
    }
    Ok(SharedFramePlan {
        device: device.clone(),
        backend: backend.kind,
        mode: mode.clone(),
        interval,
        consumers,
        rejected,
        stop_when_idle: None,
        groups,
    })
}

/// Rank a mode for all consumers: the smallest mode covering every stated size when all state
/// one (else the largest), then the total cost, frame rate and backend.
fn shared_rank(candidates: &[Candidate<'_>], requirements: &[FrameRequirements]) -> RankKey {
    let first = &candidates[0];
    let res = first.mode.format.resolution;
    let area = f64::from(res.width.get()) * f64::from(res.height.get());
    let sizes: Vec<Option<(u32, u32)>> = requirements
        .iter()
        .map(|req| req.min_resolution.or(req.output_resolution))
        .collect();
    let resolution = if sizes.iter().all(Option::is_some) {
        let (w, h) = sizes
            .iter()
            .flatten()
            .fold((0, 0), |(w, h), &(a, b)| (w.max(a), h.max(b)));
        if res.width.get() >= w && res.height.get() >= h {
            area
        } else {
            1e15 - area
        }
    } else {
        -area
    };
    RankKey {
        resolution,
        score: candidates
            .iter()
            .zip(requirements)
            .map(|(c, req)| cost::score(c.total, req.priority))
            .sum(),
        fps: -first.fps.unwrap_or(0.0),
        backend: match first.backend.kind {
            BackendKind::V4l2 => 0,
            _ => 1,
        },
    }
}

/// The fastest interval, or the slowest meeting every `min_fps` when all consumers prefer
/// power.
fn shared_interval(mode: &Mode, requirements: &[FrameRequirements]) -> Option<Interval> {
    let fastest = mode
        .intervals
        .iter()
        .copied()
        .max_by(|a, b| a.fps().total_cmp(&b.fps()));
    if !requirements.iter().all(|r| r.priority == Priority::Power) {
        return fastest;
    }
    let min_fps = requirements.iter().filter_map(|r| r.min_fps).max();
    mode.intervals
        .iter()
        .copied()
        .filter(|i| min_fps.is_none_or(|min| i.fps() + 0.5 >= min as f32))
        .min_by(|a, b| a.fps().total_cmp(&b.fps()))
        .or(fastest)
}

impl SharedFramePlan {
    /// Stop the camera streaming after `after` without a pull from any consumer, and start it
    /// again on the next (libcamera and V4L2; see `StyxConfig::stop_when_idle`).
    pub fn stop_when_idle(mut self, after: Duration) -> Self {
        self.stop_when_idle = Some((after, IdleStop::Release));
        self
    }

    /// Like [`SharedFramePlan::stop_when_idle`], but keep the camera configured while idle so
    /// it starts again quickly (libcamera; see `IdleStop::Pause`).
    pub fn pause_when_idle(mut self, after: Duration) -> Self {
        self.stop_when_idle = Some((after, IdleStop::Pause));
        self
    }

    /// Start the capture; returns one [`PlannedFrames`](super::PlannedFrames) per consumer,
    /// in order. The capture stops when the last of them is dropped or stopped.
    pub fn start(&self) -> Result<Vec<super::PlannedFrames>, CaptureError> {
        let depths: Vec<usize> = self
            .consumers
            .iter()
            .map(|p| p.queue_depth.max(1))
            .collect();
        let group_depths: Vec<usize> = self
            .groups
            .iter()
            .map(|members| members.iter().map(|&i| depths[i]).max().unwrap_or(1))
            .collect();
        // Device buffers: each group's queued captured frames, and each consumer's queued
        // frames and the one it is working on (frames that are views of the capture hold its
        // buffers), so no consumer starves the camera of buffers.
        let held = group_depths.iter().sum::<usize>() + depths.iter().sum::<usize>() + depths.len();
        let mut config = StyxConfig::new()
            .capture_queue_depth(1)
            .capture_extra_buffers(held);
        if let Some(level) = self.consumers.iter().find_map(|p| p.isp_pyramid_level) {
            config = config.libcamera_pyramid_level(level);
        }
        let main = self.consumers.iter().find(|p| !p.isp_second_output);
        if let Some((width, height)) = main.and_then(|p| p.isp_output) {
            config = config.libcamera_output_size(width, height);
        }
        if let Some((width, height)) = self
            .consumers
            .iter()
            .find(|p| p.isp_second_output)
            .and_then(|p| p.isp_output)
        {
            config = config.libcamera_second_output(width, height);
        }
        config = match self.stop_when_idle {
            Some((after, IdleStop::Pause)) => config.pause_when_idle(after),
            Some((after, _)) => config.stop_when_idle(after),
            None => config,
        };
        let mut request = CaptureRequest::new(&self.device)
            .backend(self.backend)
            .mode(self.mode.id.clone())
            .config(config);
        if let Some(interval) = self.interval {
            request = request.interval(interval);
        }
        let capture = request.start()?;
        // A latest-frame queue of captured frames per group, and of prepared frames per member.
        let (raw_txs, raw_rxs): (Vec<_>, Vec<_>) = group_depths
            .iter()
            .map(|&depth| bounded_with(depth, QueueOverflow::DropOldest))
            .unzip();
        let shared = Arc::new(SharedCapture {
            capture: Some(capture),
            fanout: Fanout::new(raw_txs),
        });
        let mut frames: Vec<Option<super::PlannedFrames>> =
            (0..self.consumers.len()).map(|_| None).collect();
        for (group_index, (members, raw_rx)) in self.groups.iter().zip(raw_rxs).enumerate() {
            let (txs, rxs): (Vec<_>, Vec<_>) = members
                .iter()
                .map(|&i| bounded_with(depths[i], QueueOverflow::DropOldest))
                .unzip();
            let rois: Vec<RoiHandle> = members
                .iter()
                .map(|&i| {
                    let roi = RoiHandle::default();
                    roi.set(self.consumers[i].requirements.roi);
                    roi
                })
                .collect();
            // Alone, a consumer's region is applied while preparing (a JPEG decoder skips the
            // rows below it); shared, each member crops the frame it gets.
            let alone = members.len() == 1;
            let group = PreparedGroup::new(
                &self.consumers[members[0]],
                shared.clone(),
                group_index,
                raw_rx,
                Fanout::new(txs),
                if alone {
                    rois[0].clone()
                } else {
                    RoiHandle::default()
                },
            );
            for (member, ((&consumer, rx), roi)) in members.iter().zip(rxs).zip(rois).enumerate() {
                frames[consumer] = Some(super::PlannedFrames::branch(
                    &self.consumers[consumer],
                    group.clone(),
                    member,
                    rx,
                    roi,
                    !alone,
                ));
            }
        }
        Ok(frames.into_iter().flatten().collect())
    }
}

/// Consumers whose frames are prepared the same way: same requirements apart from the region of
/// interest (applied per consumer, as a crop of the shared frame), on the same route.
fn prepare_groups(consumers: &[FramePlan]) -> Vec<Vec<usize>> {
    let key = |plan: &FramePlan| {
        let mut req = plan.requirements.clone();
        req.roi = None;
        req
    };
    let same = |a: &FramePlan, b: &FramePlan| {
        key(a) == key(b)
            && a.route.same_as(&b.route)
            && a.decode_scale == b.decode_scale
            && a.isp_output == b.isp_output
            && a.isp_second_output == b.isp_second_output
            && a.isp_pyramid_level == b.isp_pyramid_level
    };
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, plan) in consumers.iter().enumerate() {
        match groups.iter_mut().find(|g| same(&consumers[g[0]], plan)) {
            Some(group) => group.push(i),
            None => groups.push(vec![i]),
        }
    }
    groups
}

/// Pull-based fan-out of frames to several receivers: the first to ask reads the source and
/// leaves a zero-copy share of each frame for the others.
pub(crate) struct Fanout {
    branches: Vec<BoundedTx<FrameLease>>,
    /// Held by the branch reading the source; the others wait on their own queues.
    puller: Mutex<()>,
}

impl Fanout {
    pub(crate) fn new(branches: Vec<BoundedTx<FrameLease>>) -> Self {
        Self {
            branches,
            puller: Mutex::new(()),
        }
    }

    /// Next frame for branch `index` (receiving on `rx`), waiting up to `wait`. `source(wait)`
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
            if let Some(_pulling) = self.puller.try_lock() {
                // A frame the source already holds is newer than any share queued for us:
                // hand it to every branch first so no branch serves a stale frame.
                match source(Duration::ZERO) {
                    RecvOutcome::Data(frame) => drop(self.fan_out(frame, None)),
                    RecvOutcome::Closed => self.close(),
                    RecvOutcome::Empty => {}
                }
            }
            match rx.recv() {
                RecvOutcome::Data(frame) => return RecvOutcome::Data(frame),
                RecvOutcome::Closed => return RecvOutcome::Closed,
                RecvOutcome::Empty => {}
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return RecvOutcome::Empty;
            }
            if let Some(_pulling) = self.puller.try_lock() {
                match source(remaining.min(Duration::from_millis(50))) {
                    RecvOutcome::Data(frame) => {
                        return RecvOutcome::Data(self.fan_out(frame, Some(index)));
                    }
                    RecvOutcome::Empty => {}
                    RecvOutcome::Closed => {
                        self.close();
                        return rx.recv();
                    }
                }
            } else {
                // Another branch is reading the source and will leave us a share.
                match rx.recv_timeout(remaining.min(Duration::from_millis(5))) {
                    RecvWaitOutcome::Data(frame) => return RecvOutcome::Data(frame),
                    RecvWaitOutcome::Closed => return RecvOutcome::Closed,
                    RecvWaitOutcome::Timeout => {}
                }
            }
        }
    }

    /// Send shares of `frame` to every branch but `keep`, returning the frame itself.
    fn fan_out(&self, frame: FrameLease, keep: Option<usize>) -> FrameLease {
        if self.branches.len() == 1 && keep.is_some() {
            return frame;
        }
        let frame = frame.into_shareable();
        for (other, tx) in self.branches.iter().enumerate() {
            if Some(other) != keep
                && let Some(share) = frame.share()
            {
                tx.send(share);
            }
        }
        frame
    }

    fn close(&self) {
        self.branches.iter().for_each(BoundedTx::close);
    }
}

/// The capture behind a [`SharedFramePlan`]; stopped when the last branch goes.
pub(crate) struct SharedCapture {
    capture: Option<CaptureHandle>,
    /// One branch per group of consumers.
    fanout: Fanout,
}

impl SharedCapture {
    pub(crate) fn capture(&self) -> &CaptureHandle {
        self.capture.as_ref().expect("capture runs until dropped")
    }

    /// Next captured frame for group `index` (receiving on `rx`), waiting up to `wait`.
    pub(crate) fn next(
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
}

impl Drop for SharedCapture {
    fn drop(&mut self) {
        if let Some(capture) = self.capture.take() {
            capture.stop();
        }
    }
}
