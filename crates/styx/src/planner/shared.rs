//! Several consumers of one camera: one capture, and a branch per consumer that prepares its
//! own frames (decode, scale, ROI, pyramid).
//!
//! Frames are pulled, not pushed: the consumer that asks first reads the capture and leaves a
//! zero-copy share of the frame in every other branch's latest-frame queue. Each branch then
//! prepares only the frames its consumer takes, and nothing reads the camera while nobody asks
//! (so `StyxConfig::stop_when_idle` can stop it).

use std::fmt;
use std::time::Duration;

use styx_codec::CodecRegistryHandle;
use styx_core::prelude::*;

use super::region_shared;
use super::request::{FrameRate, FrameRequest};
use super::routes::{self, Candidate, backend_name, describe};
use super::session::{SharedSession, same_preparation};
use super::{
    FramePlan, PlanError, PlanRejection, RankKey, StepKind, cost, default_registry, no_candidates,
    plan_from, rate, strict_reason,
};
use crate::BackendKind;
use crate::capture_api::{CaptureError, CaptureRequest, IdleStop, StyxConfig};
use crate::prelude::{Interval, Mode, ProbedBackend, ProbedDevice};

/// One capture serving several consumers, each with its own [`FramePlan`].
#[derive(Clone)]
pub struct SharedFramePlan {
    pub device: ProbedDevice,
    pub backend: BackendKind,
    pub mode: Mode,
    pub interval: Option<Interval>,
    /// One plan per consumer, in the order the requests were given, all on this capture.
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

/// Plan `requests` (one per consumer) on one capture of `device`, using the default codec
/// registry. Each consumer gets its own format, size, delivery and route; they share the mode
/// and the frame rate (the one every request accepts). Takes [`FrameRequest`]s (or the
/// deprecated `FrameRequirements`).
pub fn plan_many<R: Clone + Into<FrameRequest>>(
    device: &ProbedDevice,
    requests: &[R],
) -> Result<SharedFramePlan, PlanError> {
    plan_many_with(device, requests, &default_registry()?)
}

/// [`plan_many`] with `registry` to find decoders.
pub fn plan_many_with<R: Clone + Into<FrameRequest>>(
    device: &ProbedDevice,
    requests: &[R],
    registry: &CodecRegistryHandle,
) -> Result<SharedFramePlan, PlanError> {
    let requests: Vec<FrameRequest> = requests.iter().cloned().map(Into::into).collect();
    plan_shared(device, &requests, registry)
}

fn plan_shared(
    device: &ProbedDevice,
    requests: &[FrameRequest],
    registry: &CodecRegistryHandle,
) -> Result<SharedFramePlan, PlanError> {
    if requests.is_empty() {
        return Err(PlanError::NoConsumers);
    }
    let rates: Vec<FrameRate> = requests.iter().map(|r| r.fps).collect();
    let fps = rate::combine(&rates).map_err(PlanError::FrameRate)?;
    let mut rejected = Vec::new();
    let mut best: Option<(RankKey, &ProbedBackend, &Mode, Vec<Candidate<'_>>)> = None;
    for backend in &device.backends {
        // Every consumer's backend override must allow it.
        if requests
            .iter()
            .any(|req| req.backend.is_some_and(|b| b != backend.kind))
        {
            continue;
        }
        for mode in &backend.descriptor.modes {
            if let Err(reason) = rate::check(mode, fps) {
                rejected.push(PlanRejection {
                    candidate: describe(backend, mode),
                    reason,
                });
                continue;
            }
            let candidates: Result<Vec<_>, String> = requests
                .iter()
                .enumerate()
                .map(|(i, req)| {
                    // A native PiSP makes the other processed format on an output of its own.
                    match routes::isp_format_candidate(backend, mode, req, registry) {
                        Some(c) => Ok(c),
                        None => routes::candidate(backend, mode, req, registry),
                    }
                    .map_err(|reason| format!("consumer {i}: {reason}"))
                })
                .collect();
            // Strict consumers take only modes that meet their request.
            let candidates = candidates.and_then(|candidates| {
                for (i, (candidate, req)) in candidates.iter().zip(requests).enumerate() {
                    let unmet = candidate.unmet(req);
                    if req.strict && !unmet.is_empty() {
                        return Err(format!("consumer {i}: {}", strict_reason(&unmet)));
                    }
                }
                Ok(candidates)
            });
            match candidates {
                Ok(candidates) => {
                    let key = shared_rank(&candidates, requests);
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
        return Err(no_candidates(fps, rejected));
    };
    // The ISP has two outputs: the main one at the larger size consumers want and the second
    // at the smaller one (a native PiSP also in either processed format). Consumers that fit
    // neither (a third size or format, or with the second output taken by an ISP pyramid) get
    // the mode's size and format and scale or convert on their own.
    let res = mode.format.resolution;
    let mode_size = (res.width.get(), res.height.get());
    let code = mode.format.code;
    let delivered = |c: &Candidate<'_>| {
        (
            c.isp_output.unwrap_or(mode_size),
            c.isp_format.unwrap_or(code),
        )
    };
    let outputs_of = |candidates: &[Candidate<'_>]| {
        let mut outs: Vec<((u32, u32), FourCc)> = candidates.iter().map(delivered).collect();
        outs.sort_by_key(|&((w, h), c)| {
            (std::cmp::Reverse(u64::from(w) * u64::from(h)), c != code)
        });
        outs.dedup();
        outs
    };
    let isp_two = (routes::has_isp_second_output(backend)
        || routes::native_isp_outputs(backend, mode))
        && candidates.iter().all(|c| c.isp_pyramid_level.is_none());
    let fits = |outs: &[((u32, u32), FourCc)]| outs.len() == 1 || (outs.len() == 2 && isp_two);
    let rejection = |reason: String| PlanError::NoCandidates {
        rejected: vec![PlanRejection {
            candidate: describe(backend, mode),
            reason,
        }],
    };
    // First the sizes go (consumers scale on their own), then the formats (they convert).
    let mut full_size = vec![false; candidates.len()];
    if !fits(&outputs_of(&candidates)) {
        for (i, req) in requests.iter().enumerate() {
            if candidates[i].isp_output.is_some() {
                let mut full = req.clone();
                full.size = None;
                let mut candidate =
                    match routes::isp_format_candidate(backend, mode, &full, registry) {
                        Some(c) => c,
                        None => {
                            routes::candidate(backend, mode, &full, registry).map_err(rejection)?
                        }
                    };
                candidate.notes.push(
                    "other consumers of this capture need different sizes, so the ISP delivers \
                     the mode's size"
                        .into(),
                );
                candidates[i] = candidate;
                full_size[i] = true;
            }
        }
    }
    if !fits(&outputs_of(&candidates)) {
        for (i, req) in requests.iter().enumerate() {
            if candidates[i].isp_format.is_some() {
                let mut req = req.clone();
                if full_size[i] {
                    req.size = None;
                }
                candidates[i] =
                    routes::candidate(backend, mode, &req, registry).map_err(rejection)?;
            }
        }
    }
    let outs = outputs_of(&candidates);
    let two_outputs = outs.len() == 2 && isp_two;
    let second_output = two_outputs.then(|| outs[1]);
    // Every consumer's regions from the one PiSP: the main output, the second, extra passes.
    if requests.len() > 1 {
        let second_taken =
            second_output.is_some() || candidates.iter().any(|c| c.isp_pyramid_level.is_some());
        region_shared::assign(&mut candidates, requests, second_taken).map_err(rejection)?;
    }
    let interval = rate::pick(mode, fps);
    let consumers: Vec<FramePlan> = candidates
        .into_iter()
        .zip(requests)
        .map(|(candidate, req)| {
            let second = second_output.is_some_and(|out| delivered(&candidate) == out);
            let mut plan = plan_from(device, candidate, req, interval, Vec::new());
            if second {
                plan.isp_second_output = true;
                if let Some(step) = plan.steps.iter_mut().find(|s| s.kind == StepKind::Scale) {
                    step.detail = format!("{}, on its second output", step.detail);
                } else {
                    plan.notes.push("from the ISP's second output".into());
                }
            }
            plan
        })
        .collect();
    let mut consumers: Vec<FramePlan> = consumers;
    // The ISP outputs shared out above may leave a strict consumer without its size.
    if let Some((i, plan)) = consumers
        .iter()
        .enumerate()
        .find(|(_, plan)| plan.request.strict && !plan.unmet.is_empty())
    {
        return Err(PlanError::NoCandidates {
            rejected: vec![PlanRejection {
                candidate: describe(backend, mode),
                reason: format!(
                    "consumer {i}: {} (other consumers need the ISP's outputs)",
                    strict_reason(&plan.unmet)
                ),
            }],
        });
    }
    let config = region_shared::region_config(
        &consumers,
        outputs_config(&consumers, mode, StyxConfig::default()),
    );
    region_shared::mark_second_output(&mut consumers, &config);
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
fn shared_rank(candidates: &[Candidate<'_>], requests: &[FrameRequest]) -> RankKey {
    let first = &candidates[0];
    let res = first.mode.format.resolution;
    let area = f64::from(res.width.get()) * f64::from(res.height.get());
    let sizes: Vec<Option<(u32, u32)>> = requests
        .iter()
        .map(|req| req.min_size.or(req.size))
        .collect();
    let resolution = if sizes.iter().all(Option::is_some) {
        let (w, h) = sizes
            .iter()
            .flatten()
            .fold((0, 0), |(w, h), &(a, b)| (w.max(a), h.max(b)));
        if res.width.get() >= w && res.height.get() >= h {
            // The frames the consumers get, after the ISP or decoders scale them: an ISP scaling
            // a wide mode beats a smaller mode of another aspect ratio.
            candidates
                .iter()
                .map(|c| {
                    let scale = u32::from(c.decode_scale.max(1));
                    let (w, h) = c.isp_output.unwrap_or((
                        res.width.get().div_ceil(scale),
                        res.height.get().div_ceil(scale),
                    ));
                    f64::from(w) * f64::from(h)
                })
                .sum()
        } else {
            1e15 - area
        }
    } else {
        -area
    };
    RankKey {
        resolution,
        score: candidates.iter().map(|c| cost::score(c.total)).sum(),
        fps: -first.fps.unwrap_or(0.0),
        backend: match first.backend.kind {
            BackendKind::V4l2 => 0,
            _ => 1,
        },
        isp_formats: candidates.iter().filter(|c| c.isp_format.is_some()).count() as u8,
    }
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

    /// Capture settings to start from, for every consumer (see [`FramePlan::config`]).
    pub fn config(mut self, config: StyxConfig) -> Self {
        for consumer in &mut self.consumers {
            consumer.config = Some(config.clone());
        }
        self
    }

    /// Put frames consumers' plans decode or copy into memfd buffers, for other processes (see
    /// [`FramePlan::exportable`]).
    pub fn exportable(mut self) -> Self {
        for consumer in &mut self.consumers {
            consumer.exportable = true;
        }
        self
    }

    /// Start the capture; returns one [`Frames`](super::Frames) per consumer,
    /// in order. The capture stops when the last of them is dropped or stopped.
    pub fn start(&self) -> Result<Vec<super::Frames>, CaptureError> {
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
        let session = SharedSession::new(self.capture_request(held).start()?);
        let mut frames: Vec<Option<super::Frames>> =
            (0..self.consumers.len()).map(|_| None).collect();
        for members in &self.groups {
            // Alone, a consumer's region is applied while preparing (a JPEG decoder skips the
            // rows below it); shared, each member crops the frame it gets.
            let share = members.len() > 1;
            for &consumer in members {
                frames[consumer] = Some(session.attach(&self.consumers[consumer], share));
            }
        }
        Ok(frames.into_iter().flatten().collect())
    }

    /// Start the capture with buffers for `consumers` consumers that each hold up to `held`
    /// frames beyond their queues (e.g. frames other processes hold); consumers join it with
    /// [`SharedSession::attach`].
    pub(crate) fn start_session(
        &self,
        consumers: usize,
        held: usize,
    ) -> Result<SharedSession, CaptureError> {
        let depth = self
            .consumers
            .iter()
            .map(|p| p.queue_depth.max(1))
            .max()
            .unwrap_or(1);
        let per_consumer = depth + held;
        let request = self.capture_request(consumers.max(1) * per_consumer);
        Ok(SharedSession::new(request.start()?))
    }

    /// What the capture is started with: consumers whose plans have the same key can join a
    /// running capture of this plan.
    pub(crate) fn setup_key(&self) -> String {
        let output = |p: &FramePlan| (p.isp_output, p.isp_format);
        let second = self
            .consumers
            .iter()
            .find(|p| p.isp_second_output)
            .map(output);
        let main = self
            .consumers
            .iter()
            .find(|p| !p.isp_second_output)
            .map(output);
        let pyramid = self.consumers.iter().find_map(|p| p.isp_pyramid_level);
        let region = region_shared::layout_key(&self.consumers);
        format!(
            "{:?} {:?} {:?} main={main:?} second={second:?} pyramid={pyramid:?} region={region} \
             idle={:?}",
            self.backend, self.mode.id, self.interval, self.stop_when_idle
        )
    }

    /// The capture request: this plan's mode and interval, the ISP outputs its consumers use,
    /// and `held` device buffers beyond the queue.
    fn capture_request(&self, held: usize) -> CaptureRequest<'_> {
        // Consumers' own capture settings: the first that has some.
        let base = self.consumers.iter().find_map(|p| p.config.clone());
        let mut config = base
            .unwrap_or_default()
            .capture_queue_depth(1)
            .capture_extra_buffers(held);
        config = region_shared::region_config(
            &self.consumers,
            outputs_config(&self.consumers, &self.mode, config),
        );
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
        request
    }
}

/// `config` with the ISP outputs `consumers` of a shared capture of `mode` use: the main
/// output's size and format, the second output's, the pyramid level.
fn outputs_config(consumers: &[FramePlan], mode: &Mode, mut config: StyxConfig) -> StyxConfig {
    if let Some(level) = consumers.iter().find_map(|p| p.isp_pyramid_level) {
        config = config
            .libcamera_pyramid_level(level)
            .native_pyramid_level(level);
    }
    let main = consumers.iter().find(|p| !p.isp_second_output);
    if let Some((width, height)) = main.and_then(|p| p.isp_output) {
        config = config
            .libcamera_output_size(width, height)
            .native_output_size(width, height);
    }
    if let Some(format) = main.and_then(|p| p.isp_format) {
        config = config.native_output_format(format);
    }
    if let Some(second) = consumers.iter().find(|p| p.isp_second_output) {
        let res = mode.format.resolution;
        let (width, height) = second
            .isp_output
            .unwrap_or((res.width.get(), res.height.get()));
        if second.isp_output.is_some() {
            config = config.libcamera_second_output(width, height);
        }
        let format = second.isp_format.unwrap_or(mode.format.code);
        config = config.native_second_output(width, height, format);
    }
    config
}

/// Consumers whose frames are prepared the same way: same requests apart from the region of
/// interest (applied per consumer, as a crop of the shared frame), on the same route.
fn prepare_groups(consumers: &[FramePlan]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, plan) in consumers.iter().enumerate() {
        match groups
            .iter_mut()
            .find(|g| same_preparation(&consumers[g[0]], plan))
        {
            Some(group) => group.push(i),
            None => groups.push(vec![i]),
        }
    }
    groups
}
