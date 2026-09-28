//! One camera of a camera service: its clients and the shared capture planned for them.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use parking_lot::Mutex;
use styx_core::prelude::*;

use super::{Counters, ServiceConfig};
use crate::capture_api::IdleStop;
use crate::planner::{PlanError, PlannedFrames, SharedFramePlan, SharedSession, plan_many};
use crate::prelude::ProbedDevice;

/// Clients a capture is sized for beyond those connected when it starts, so another can join
/// without restarting it.
const SPARE_CLIENTS: usize = 1;

pub(super) type FramesSlot = Arc<Mutex<Option<PlannedFrames>>>;

pub(super) struct Camera {
    pub(super) device: ProbedDevice,
    state: Mutex<State>,
}

struct State {
    clients: Vec<Client>,
    running: Option<Running>,
    next_id: u64,
}

struct Client {
    id: u64,
    requirements: FrameRequirements,
    /// Its frames on the running capture; replaced when the capture restarts.
    frames: FramesSlot,
}

struct Running {
    session: SharedSession,
    plan: SharedFramePlan,
    /// What the capture was started with (see [`SharedFramePlan::setup_key`]).
    setup: String,
    /// Clients its buffers are sized for.
    capacity: usize,
}

impl Camera {
    pub(super) fn new(device: ProbedDevice) -> Self {
        Self {
            device,
            state: Mutex::new(State {
                clients: Vec::new(),
                running: None,
                next_id: 0,
            }),
        }
    }

    pub(super) fn clients(&self) -> usize {
        self.state.lock().clients.len()
    }

    pub(super) fn plan(&self) -> Option<String> {
        let state = self.state.lock();
        state
            .running
            .as_ref()
            .map(|running| running.plan.to_string())
    }

    /// Plan for the connected clients plus `requirements`; attach the new client to the running
    /// capture if it fits, else restart the capture for everyone.
    pub(super) fn join(
        &self,
        requirements: FrameRequirements,
        config: &ServiceConfig,
        counters: &Counters,
    ) -> Result<(u64, String, FramesSlot), String> {
        let mut state = self.state.lock();
        let mut all: Vec<FrameRequirements> = state
            .clients
            .iter()
            .map(|c| c.requirements.clone())
            .collect();
        all.push(requirements.clone());
        let plan = self.plan_for(&all, config).map_err(|err| describe(&err))?;
        let new = plan.consumers.last().expect("one plan per client");
        let text = new.to_string();
        let setup = plan.setup_key();
        let fits = state
            .running
            .as_ref()
            .is_some_and(|running| running.setup == setup && running.capacity >= all.len());
        let frames = if fits {
            let running = state.running.as_mut().expect("checked above");
            let frames = running.session.attach(new, true);
            running.plan = plan;
            frames
        } else {
            let restarting = state.running.is_some();
            match restart(&mut state, plan, all.len(), config) {
                Ok(frames) => {
                    if restarting {
                        counters.restarts.fetch_add(1, Ordering::Relaxed);
                    }
                    frames
                }
                Err(err) => {
                    // Keep serving the clients that were there.
                    let existing = &all[..all.len() - 1];
                    if !existing.is_empty()
                        && let Ok(plan) = self.plan_for(existing, config)
                    {
                        let _ = restart(&mut state, plan, existing.len(), config);
                    }
                    return Err(err);
                }
            }
        };
        let id = state.next_id;
        state.next_id += 1;
        let slot = Arc::new(Mutex::new(Some(frames)));
        state.clients.push(Client {
            id,
            requirements,
            frames: slot.clone(),
        });
        Ok((id, text, slot))
    }

    fn plan_for(
        &self,
        requirements: &[FrameRequirements],
        config: &ServiceConfig,
    ) -> Result<SharedFramePlan, PlanError> {
        let plan = plan_many(&self.device, requirements)?.exportable();
        Ok(match config.idle {
            Some((after, IdleStop::Pause)) => plan.pause_when_idle(after),
            Some((after, _)) => plan.stop_when_idle(after),
            None => plan,
        })
    }

    pub(super) fn leave(&self, id: u64, config: &ServiceConfig) {
        let mut state = self.state.lock();
        if let Some(i) = state.clients.iter().position(|c| c.id == id) {
            let client = state.clients.remove(i);
            client.frames.lock().take();
        }
        // Paused cameras wait for the next client; otherwise the camera goes with the last one.
        let pause = matches!(config.idle, Some((_, IdleStop::Pause)));
        if state.clients.is_empty() && !pause {
            state.running = None;
        }
    }

    pub(super) fn set_roi(&self, id: u64, roi: Option<FrameRect>, frames: &FramesSlot) {
        if let Some(client) = self.state.lock().clients.iter_mut().find(|c| c.id == id) {
            client.requirements.roi = roi;
        }
        if let Some(frames) = frames.lock().as_ref() {
            frames.roi().set(roi);
        }
    }

    /// Disconnect every client and stop the capture.
    pub(super) fn shut_down(&self) {
        let mut state = self.state.lock();
        for client in state.clients.drain(..) {
            client.frames.lock().take();
        }
        state.running = None;
    }
}

/// Start `plan`'s capture in place of the running one: the connected clients get their frames
/// from it (in plan order), and the frames for the plan's last consumer (the new client) are
/// returned.
fn restart(
    state: &mut State,
    plan: SharedFramePlan,
    clients: usize,
    config: &ServiceConfig,
) -> Result<PlannedFrames, String> {
    // The old capture must let the camera go before the new one opens it.
    for client in &state.clients {
        client.frames.lock().take();
    }
    state.running = None;
    let capacity = clients + SPARE_CLIENTS;
    let session = plan
        .start_session(capacity, config.max_in_flight)
        .map_err(|err| format!("the camera did not start: {err}"))?;
    for (client, consumer) in state.clients.iter().zip(&plan.consumers) {
        *client.frames.lock() = Some(session.attach(consumer, true));
    }
    let frames = session.attach(plan.consumers.last().expect("a consumer"), true);
    state.running = Some(Running {
        session,
        setup: plan.setup_key(),
        plan,
        capacity,
    });
    Ok(frames)
}

fn describe(err: &PlanError) -> String {
    match err {
        PlanError::NoCandidates { rejected } => {
            let reasons: Vec<String> = rejected
                .iter()
                .take(6)
                .map(|r| format!("{}: {}", r.candidate, r.reason))
                .collect();
            format!("{err}: {}", reasons.join("; "))
        }
        _ => err.to_string(),
    }
}

/// Refuse requirements no camera needs and that would make the service allocate or spend without
/// bound: another process wrote them.
pub(super) fn check_request(req: &FrameRequirements) -> Result<(), String> {
    const MAX_SIZE: u32 = 16_384;
    let size_ok = |size: Option<(u32, u32)>| {
        size.is_none_or(|(w, h)| (1..=MAX_SIZE).contains(&w) && (1..=MAX_SIZE).contains(&h))
    };
    if !size_ok(req.min_resolution)
        || !size_ok(req.max_resolution)
        || !size_ok(req.output_resolution)
    {
        return Err(format!("sizes must be 1 to {MAX_SIZE} pixels"));
    }
    if let Some(roi) = req.roi
        && (roi.x.saturating_add(roi.width) > MAX_SIZE
            || roi.y.saturating_add(roi.height) > MAX_SIZE)
    {
        return Err("region of interest is outside any frame".into());
    }
    if req
        .stride_alignment
        .is_some_and(|a| !a.is_power_of_two() || a > 4096)
    {
        return Err("stride alignment must be a power of two up to 4096".into());
    }
    if req.pyramid.is_some_and(|p| p.levels > 4) {
        return Err("at most 4 pyramid levels".into());
    }
    if req.min_fps.is_some_and(|fps| fps > 1000) {
        return Err("minimum frame rate above 1000 fps".into());
    }
    let o = &req.overrides;
    if o.queue_depth.is_some_and(|d| !(1..=8).contains(&d)) {
        return Err("queue depth must be 1 to 8".into());
    }
    if o.decode_threads.is_some_and(|t| t > 64) {
        return Err("at most 64 decode threads".into());
    }
    let names = o.backend.iter().chain(&o.decoder).chain(&o.forbid);
    if names.clone().count() > 18 || names.clone().any(|n| n.len() > 64) {
        return Err("override names are too long".into());
    }
    Ok(())
}

/// How long a client thread waits for frames per poll.
pub(super) const FRAME_WAIT: Duration = Duration::from_millis(50);
