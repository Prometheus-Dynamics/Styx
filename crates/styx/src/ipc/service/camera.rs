//! One camera of a camera service: its clients and the shared capture planned for them.

mod controls;
mod priority;

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use parking_lot::Mutex;
use styx_core::prelude::*;

use self::priority::Planned;
use super::{Counters, ServiceConfig};
use crate::capture_api::IdleStop;
use crate::ipc::ClientPriority;
use crate::ipc::wire::ClientToken;
use crate::planner::{
    Delivered, FrameRate, FrameRequest, Frames, PlanError, SharedFramePlan, SharedSession,
    plan_many,
};
use crate::prelude::ProbedDevice;

/// Clients a capture is sized for beyond those connected when it starts, so another can join
/// without restarting it.
const SPARE_CLIENTS: usize = 1;

pub(super) type FramesSlot = Arc<Mutex<Option<Frames>>>;

pub(super) struct Camera {
    pub(super) device: ProbedDevice,
    state: Mutex<State>,
}

struct State {
    clients: Vec<Client>,
    running: Option<Running>,
    next_id: u64,
    /// Controls clients set (backend ids and values), applied again when the capture restarts.
    controls: Vec<(ControlId, ControlValue)>,
    /// A frame rate a client set that the camera cannot change while streaming: every
    /// client's request is planned at it.
    fps_override: Option<u32>,
    /// Control connections that subscribed to changes (their key, a duplicate of the socket).
    subscribers: Vec<(u64, OwnedFd)>,
    next_subscriber: u64,
}

struct Client {
    id: u64,
    /// Proves the client on control requests (see `wire::ClientToken`).
    token: u64,
    request: FrameRequest,
    /// Low priority ([`ClientPriority::Low`]): never changes the other clients' frames.
    low: bool,
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
    /// The request each consumer of `plan` was planned with, in order (a low-priority client's
    /// may be a share of another's).
    requests: Vec<FrameRequest>,
    /// The client each consumer of `plan` serves, in order.
    owners: Vec<u64>,
}

impl Running {
    /// Forget a low-priority client's consumer: it never shaped the capture, so the next
    /// low-priority client is planned without it.
    fn forget(&mut self, id: u64) {
        while let Some(i) = self.owners.iter().position(|&owner| owner == id) {
            self.owners.remove(i);
            self.requests.remove(i);
            self.plan.consumers.remove(i);
        }
    }
}

/// The client each consumer of a plan serves: `consumers` gives each client's consumer, in
/// client order, the new client (id `new`) last.
fn owners(state: &State, consumers: &[Option<usize>], new: u64) -> Vec<u64> {
    let mut owners = vec![u64::MAX; consumers.iter().flatten().count()];
    let ids = state.clients.iter().map(|c| c.id).chain([new]);
    for (id, consumer) in ids.zip(consumers) {
        if let Some(k) = consumer {
            owners[*k] = id;
        }
    }
    owners
}

impl Camera {
    pub(super) fn new(device: ProbedDevice) -> Self {
        Self {
            device,
            state: Mutex::new(State {
                clients: Vec::new(),
                running: None,
                next_id: 0,
                controls: Vec::new(),
                fps_override: None,
                subscribers: Vec::new(),
                next_subscriber: 0,
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

    /// Plan for the connected clients plus `request`; attach the new client to the running
    /// capture if it fits, else restart the capture for everyone. A low-priority client never
    /// restarts a capture its normal clients use, and never changes their frames (see
    /// [`Camera::plan_clients`]).
    pub(super) fn join(
        &self,
        request: FrameRequest,
        priority: ClientPriority,
        config: &ServiceConfig,
        counters: &Counters,
    ) -> Result<(ClientToken, String, Delivered, FramesSlot), String> {
        let mut state = self.state.lock();
        let low = priority == ClientPriority::Low;
        let (frames, text, delivered) =
            if low && state.running.is_some() && state.clients.iter().any(|c| !c.low) {
                self.attach_low(&mut state, &request, config)?
            } else {
                self.join_planned(&mut state, &request, low, config, counters)?
            };
        let id = state.next_id;
        state.next_id += 1;
        let token = new_token(id);
        let slot = Arc::new(Mutex::new(Some(frames)));
        state.clients.push(Client {
            id,
            token,
            request,
            low,
            frames: slot.clone(),
        });
        Ok((ClientToken { id, token }, text, delivered, slot))
    }

    /// Plan every client and the new one (see [`Camera::plan_clients`]); attach it to the
    /// running capture if that fits, else restart the capture for everyone.
    fn join_planned(
        &self,
        state: &mut State,
        request: &FrameRequest,
        low: bool,
        config: &ServiceConfig,
        counters: &Counters,
    ) -> Result<(Frames, String, Delivered), String> {
        let mut all: Vec<(FrameRequest, bool)> = state
            .clients
            .iter()
            .map(|c| (c.request.clone(), c.low))
            .collect();
        all.push((request.clone(), low));
        let fps = state.fps_override;
        // Low-priority clients first keep the running capture as it is set up, so they never
        // make a normal client's join restart it.
        let running = state
            .running
            .as_ref()
            .map(|r| (r.setup.clone(), r.capacity));
        let kept = match &running {
            Some((setup, _)) => Some(
                self.plan_clients(&all, fps, Some(setup), config)
                    .map_err(|err| describe(&err))?,
            )
            .filter(|planned| planned.plan.setup_key() == *setup),
            None => None,
        };
        let planned = match kept {
            Some(planned) => planned,
            None => self
                .plan_clients(&all, fps, None, config)
                .map_err(|err| describe(&err))?,
        };
        let Some(Some(index)) = planned.consumers.last().copied() else {
            return Err(LOW_REFUSED.into());
        };
        let new = &planned.plan.consumers[index];
        let text = new.to_string();
        let delivered = new.delivered();
        let fits = running.is_some_and(|(setup, capacity)| {
            setup == planned.plan.setup_key() && capacity >= planned.plan.consumers.len()
        });
        let frames = if fits {
            let owners = owners(state, &planned.consumers, state.next_id);
            let running = state.running.as_mut().expect("checked above");
            let frames = running.session.attach(new, true);
            running.plan = planned.plan;
            running.requests = planned.requests;
            running.owners = owners;
            frames
        } else {
            let restarting = state.running.is_some();
            match restart(state, planned, config) {
                Ok(frames) => {
                    if restarting {
                        counters.restarts.fetch_add(1, Ordering::Relaxed);
                    }
                    frames.expect("a consumer for the new client")
                }
                Err(err) => {
                    // Keep serving the clients that were there.
                    let existing = &all[..all.len() - 1];
                    if !existing.is_empty()
                        && let Ok(planned) = self.plan_clients(existing, fps, None, config)
                    {
                        let _ = restart(state, planned, config);
                    }
                    return Err(err);
                }
            }
        };
        Ok((frames, text, delivered))
    }

    /// A plan for `request` (at `fps` when a client set a frame rate the camera cannot change
    /// while streaming).
    fn plan_for(
        &self,
        request: &[FrameRequest],
        fps: Option<u32>,
        config: &ServiceConfig,
    ) -> Result<SharedFramePlan, PlanError> {
        let at_rate: Vec<FrameRequest>;
        let request = match fps {
            Some(fps) => {
                at_rate = request
                    .iter()
                    .map(|r| {
                        let mut r = r.clone();
                        r.fps = FrameRate::Exactly(fps);
                        r
                    })
                    .collect();
                &at_rate
            }
            None => request,
        };
        let plan = plan_many(&self.device, request)?.exportable();
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
            if client.low
                && let Some(running) = state.running.as_mut()
            {
                running.forget(id);
            }
        }
        // Paused cameras wait for the next client; otherwise the camera goes with the last one.
        let pause = matches!(config.idle, Some((_, IdleStop::Pause)));
        if state.clients.is_empty() && !pause {
            state.running = None;
        }
    }

    /// A client's regions of interest now (region 0 first; empty: the whole frame).
    pub(super) fn set_roi(&self, id: u64, regions: &[FrameRect], frames: &FramesSlot) {
        if let Some(client) = self.state.lock().clients.iter_mut().find(|c| c.id == id) {
            client.request.roi = regions.first().copied();
            client.request.extra_regions = regions.iter().skip(1).copied().collect();
        }
        if let Some(frames) = frames.lock().as_ref() {
            frames.roi().set_regions(regions);
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

/// A client's token: not guessable by other processes (a fresh random hash per client).
fn new_token(id: u64) -> u64 {
    use std::hash::BuildHasher;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    std::collections::hash_map::RandomState::new().hash_one((id, nanos, std::process::id()))
}

/// Start the `planned` capture in place of the running one: each connected client gets its
/// frames from the consumer the plan gives it (none: a low-priority client left without frames
/// until the next restart), the controls clients set are applied again, and when the plan has
/// an entry more than there are clients, its frames (for the new client) are returned.
fn restart(
    state: &mut State,
    planned: Planned,
    config: &ServiceConfig,
) -> Result<Option<Frames>, String> {
    let owners = owners(state, &planned.consumers, state.next_id);
    let Planned {
        plan,
        consumers,
        requests,
    } = planned;
    // The old capture must let the camera go before the new one opens it.
    for client in &state.clients {
        client.frames.lock().take();
    }
    state.running = None;
    let capacity = plan.consumers.len() + SPARE_CLIENTS;
    let session = plan
        .start_session(capacity, config.max_in_flight)
        .map_err(|err| format!("the camera did not start: {err}"))?;
    for (id, value) in &state.controls {
        if let Err(err) = session.capture().set_control(*id, value.clone()) {
            crate::trace::warn!(control = id.0, error = %err, "control not applied again");
        }
    }
    for (client, consumer) in state.clients.iter().zip(&consumers) {
        match consumer {
            Some(i) => *client.frames.lock() = Some(session.attach(&plan.consumers[*i], true)),
            None => crate::trace::warn!(
                client = client.id,
                "low-priority client left without frames until the capture restarts"
            ),
        }
    }
    let frames = consumers
        .get(state.clients.len())
        .copied()
        .flatten()
        .map(|i| session.attach(&plan.consumers[i], true));
    state.running = Some(Running {
        session,
        setup: plan.setup_key(),
        plan,
        capacity,
        requests,
        owners,
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

/// Refuse requests no camera needs and that would make the service allocate or spend without
/// bound: another process wrote them.
pub(super) fn check_request(req: &FrameRequest) -> Result<(), String> {
    const MAX_SIZE: u32 = 16_384;
    let size_ok = |size: Option<(u32, u32)>| {
        size.is_none_or(|(w, h)| (1..=MAX_SIZE).contains(&w) && (1..=MAX_SIZE).contains(&h))
    };
    if !size_ok(req.min_size) || !size_ok(req.max_size) || !size_ok(req.size) {
        return Err(format!("sizes must be 1 to {MAX_SIZE} pixels"));
    }
    if req.all_regions().iter().any(|roi| {
        roi.x.saturating_add(roi.width) > MAX_SIZE || roi.y.saturating_add(roi.height) > MAX_SIZE
    }) {
        return Err("region of interest is outside any frame".into());
    }
    if req
        .row_alignment
        .is_some_and(|a| !a.is_power_of_two() || a > 4096)
    {
        return Err("row alignment must be a power of two up to 4096".into());
    }
    if req.pyramid.is_some_and(|p| p.levels > 4) {
        return Err("at most 4 pyramid levels".into());
    }
    let fastest = match req.fps {
        FrameRate::CameraDefault => 0,
        FrameRate::Exactly(fps) | FrameRate::AtLeast(fps) => fps,
        FrameRate::Between(min, _) => min,
    };
    if fastest > 1000 {
        return Err("frame rate above 1000 fps".into());
    }
    if !(1..=8).contains(&req.delivery.queue_depth()) {
        return Err("at most 8 queued frames".into());
    }
    if req.decode_threads.is_some_and(|t| t > 64) {
        return Err("at most 64 decode threads".into());
    }
    let names = req.decoder.iter().chain(&req.forbid);
    if names.clone().count() > 17 || names.clone().any(|n| n.len() > 64) {
        return Err("decoder names are too long".into());
    }
    Ok(())
}

/// Decode `bytes` as a client's request, check it as the service does, then plan it alone and
/// shared with another client on virtual cameras (MJPEG, YUYV and NV12 modes). For fuzzing.
pub(in crate::ipc) fn fuzz_request(bytes: &[u8]) {
    use std::sync::OnceLock;

    use crate::capture_api::make_virtual_device;
    use crate::ipc::wire::{ClientMessage, decode_client};
    use crate::prelude::Mode;

    static CAMERAS: OnceLock<Vec<ProbedDevice>> = OnceLock::new();
    let Ok(ClientMessage::Request(request, _, _)) = decode_client(bytes) else {
        return;
    };
    if check_request(&request).is_err() {
        return;
    }
    let cameras = CAMERAS.get_or_init(|| {
        let mode = |code, w, h, fps| {
            Mode::with_interval(
                MediaFormat::new(code, Resolution::new(w, h).expect("size"), ColorSpace::Srgb),
                Interval::from_fps(fps).expect("rate"),
            )
        };
        vec![
            make_virtual_device(
                "usb",
                [
                    mode(FourCc::MJPG, 1280, 720, 30),
                    mode(FourCc::YUYV, 640, 480, 30),
                    mode(FourCc::YUYV, 1280, 720, 10),
                ],
            ),
            make_virtual_device("isp", [mode(FourCc::NV12, 1280, 800, 120)]),
        ]
    });
    for camera in cameras {
        let _ = plan_many(camera, std::slice::from_ref(&*request));
        let _ = plan_many(camera, &[(*request).clone(), Frames::nv12()]);
    }
}

/// Why a low-priority client is refused.
const LOW_REFUSED: &str = "a low-priority client cannot be served without changing the capture \
                           or the frames of the camera's other clients";

/// How long a client thread waits for frames per poll.
pub(super) const FRAME_WAIT: Duration = Duration::from_millis(50);
