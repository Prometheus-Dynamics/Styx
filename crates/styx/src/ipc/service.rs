//! A camera for other processes: each client asks for the frames it needs, and the service plans
//! one shared capture for all of them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_core::prelude::*;

use super::connection::{self, Connection};
use super::wire::{self, ClientMessage};
use super::{DEFAULT_MAX_IN_FLIGHT, IpcError, socket};
use crate::capture_api::IdleStop;
use crate::planner::{PlanError, PlannedFrames, SharedFramePlan, plan_many};
use crate::prelude::ProbedDevice;

/// How long a new client has to send its request.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Clients a capture is sized for beyond those connected when it starts, so a few more can join
/// without restarting it.
const SPARE_CLIENTS: usize = 1;

/// Serves one camera to [`FrameClient`](super::FrameClient)s in other processes.
///
/// Each client sends the [`FrameRequirements`] it needs ([`FrameClient::request`]). The service
/// plans one shared capture for all connected clients ([`plan_many`]): hardware scaling, both
/// ISP outputs, decoding once for clients with the same needs. A client joining a capture that
/// already fits it is attached without disturbing the others; one that needs another mode or ISP
/// setup restarts the capture once for everyone. Frames go out as descriptors (camera buffers
/// and memfds), so nothing is copied. A client gets frames only as fast as it drops them, and
/// the camera stops (by default: pauses) when no client reads.
///
/// [`FrameClient::request`]: super::FrameClient::request
#[derive(Clone)]
pub struct CameraService {
    device: ProbedDevice,
    idle: Option<(Duration, IdleStop)>,
    max_in_flight: usize,
}

impl CameraService {
    pub fn new(device: ProbedDevice) -> Self {
        Self {
            device,
            idle: Some((Duration::from_secs(2), IdleStop::Pause)),
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
        }
    }

    /// Pause the camera after `after` without a reading client (the default, after 2 s):
    /// libcamera cameras stay configured and start again in ~0.1 s; others release.
    pub fn pause_when_idle(mut self, after: Duration) -> Self {
        self.idle = Some((after, IdleStop::Pause));
        self
    }

    /// Release the camera after `after` without a reading client, and when the last one leaves.
    pub fn stop_when_idle(mut self, after: Duration) -> Self {
        self.idle = Some((after, IdleStop::Release));
        self
    }

    /// Keep the camera streaming while clients are connected, reading or not.
    pub fn keep_streaming(mut self) -> Self {
        self.idle = None;
        self
    }

    /// Frames a client may hold at once (default [`DEFAULT_MAX_IN_FLIGHT`]).
    pub fn max_in_flight(mut self, frames: usize) -> Self {
        self.max_in_flight = frames.max(1);
        self
    }

    /// Listen on the Unix socket at `path` (replacing a stale socket file there) and serve
    /// clients on background threads until the handle is stopped or dropped.
    pub fn serve(self, path: impl AsRef<Path>) -> Result<CameraServiceHandle, IpcError> {
        let path = path.as_ref().to_path_buf();
        let listener = socket::listen(&path)?;
        let service = Arc::new(Service {
            config: self,
            path,
            stopping: AtomicBool::new(false),
            state: Mutex::new(State {
                clients: Vec::new(),
                running: None,
                next_id: 0,
            }),
            counters: Counters::default(),
        });
        let accept = {
            let service = service.clone();
            std::thread::Builder::new()
                .name("styx-camera-service".into())
                .spawn(move || accept_loop(&service, &listener))?
        };
        Ok(CameraServiceHandle {
            service,
            accept: Some(accept),
        })
    }
}

/// Counters of a running [`CameraService`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CameraServiceStats {
    /// Clients connected now.
    pub clients: usize,
    /// Requests the camera could not serve.
    pub rejected: u64,
    /// Times the capture restarted because a new client needed another setup.
    pub restarts: u64,
    /// Frames sent, counted once per client.
    pub sent: u64,
    /// Frames that had to be copied into a memfd.
    pub copied: u64,
    /// Frames a client's socket had no room for.
    pub skipped: u64,
}

/// A running [`CameraService`]; stops it when dropped.
pub struct CameraServiceHandle {
    service: Arc<Service>,
    accept: Option<JoinHandle<()>>,
}

impl CameraServiceHandle {
    pub fn stats(&self) -> CameraServiceStats {
        let counters = &self.service.counters;
        CameraServiceStats {
            clients: self.service.state.lock().clients.len(),
            rejected: counters.rejected.load(Ordering::Relaxed),
            restarts: counters.restarts.load(Ordering::Relaxed),
            sent: counters.sent.load(Ordering::Relaxed),
            copied: counters.copied.load(Ordering::Relaxed),
            skipped: counters.skipped.load(Ordering::Relaxed),
        }
    }

    /// The shared plan running now, as text.
    pub fn plan(&self) -> Option<String> {
        let state = self.service.state.lock();
        state
            .running
            .as_ref()
            .map(|running| running.plan.to_string())
    }

    pub fn path(&self) -> &Path {
        &self.service.path
    }

    /// Disconnect every client and stop the camera.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.service.stopping.store(true, Ordering::Release);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let mut state = self.service.state.lock();
        for client in state.clients.drain(..) {
            client.frames.lock().take();
        }
        state.running = None;
        drop(state);
        let _ = std::fs::remove_file(&self.service.path);
    }
}

impl Drop for CameraServiceHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Default)]
struct Counters {
    rejected: AtomicU64,
    restarts: AtomicU64,
    sent: AtomicU64,
    copied: AtomicU64,
    skipped: AtomicU64,
}

struct Service {
    config: CameraService,
    path: PathBuf,
    stopping: AtomicBool,
    state: Mutex<State>,
    counters: Counters,
}

type FramesSlot = Arc<Mutex<Option<PlannedFrames>>>;

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
    session: crate::planner::SharedSession,
    plan: SharedFramePlan,
    /// What the capture was started with (see [`SharedFramePlan::setup_key`]).
    setup: String,
    /// Clients its buffers are sized for.
    capacity: usize,
}

impl Service {
    /// Plan for the connected clients plus `requirements`; attach the new client to the running
    /// capture if it fits, else restart the capture for everyone.
    fn join(&self, requirements: FrameRequirements) -> Result<(u64, String, FramesSlot), String> {
        let mut state = self.state.lock();
        let mut all: Vec<FrameRequirements> = state
            .clients
            .iter()
            .map(|c| c.requirements.clone())
            .collect();
        all.push(requirements.clone());
        let plan = self.plan(&all).map_err(|err| describe(&err))?;
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
            match self.restart(&mut state, plan, all.len()) {
                Ok(frames) => {
                    if restarting {
                        self.counters.restarts.fetch_add(1, Ordering::Relaxed);
                    }
                    frames
                }
                Err(err) => {
                    // Keep serving the clients that were there.
                    let existing = &all[..all.len() - 1];
                    if !existing.is_empty()
                        && let Ok(plan) = self.plan(existing)
                    {
                        let _ = self.restart(&mut state, plan, existing.len());
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

    fn plan(&self, requirements: &[FrameRequirements]) -> Result<SharedFramePlan, PlanError> {
        let mut plan = plan_many(&self.config.device, requirements)?.exportable();
        plan = match self.config.idle {
            Some((after, IdleStop::Pause)) => plan.pause_when_idle(after),
            Some((after, _)) => plan.stop_when_idle(after),
            None => plan,
        };
        Ok(plan)
    }

    /// Start `plan`'s capture in place of the running one: the connected clients get their
    /// frames from it (in plan order), and the frames for the plan's last consumer are returned
    /// when it has one more than there are clients.
    fn restart(
        &self,
        state: &mut State,
        plan: SharedFramePlan,
        clients: usize,
    ) -> Result<PlannedFrames, String> {
        // The old capture must let the camera go before the new one opens it.
        for client in &state.clients {
            client.frames.lock().take();
        }
        state.running = None;
        let capacity = clients + SPARE_CLIENTS;
        let session = plan
            .start_session(capacity, self.config.max_in_flight)
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

    fn leave(&self, id: u64) {
        let mut state = self.state.lock();
        if let Some(i) = state.clients.iter().position(|c| c.id == id) {
            let client = state.clients.remove(i);
            client.frames.lock().take();
        }
        // Paused cameras wait for the next client; otherwise the camera goes with the last one.
        let pause = matches!(self.config.idle, Some((_, IdleStop::Pause)));
        if state.clients.is_empty() && !pause {
            state.running = None;
        }
    }

    fn set_roi(&self, id: u64, roi: Option<FrameRect>, frames: &FramesSlot) {
        if let Some(client) = self.state.lock().clients.iter_mut().find(|c| c.id == id) {
            client.requirements.roi = roi;
        }
        if let Some(frames) = frames.lock().as_ref() {
            frames.roi().set(roi);
        }
    }
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

fn accept_loop(service: &Arc<Service>, listener: &std::os::fd::OwnedFd) {
    let mut clients: Vec<JoinHandle<()>> = Vec::new();
    while !service.stopping.load(Ordering::Acquire) {
        if !socket::readable(listener, Duration::from_millis(100)) {
            clients.retain(|c| !c.is_finished());
            continue;
        }
        while let Ok(Some(socket)) = socket::accept(listener) {
            let service = service.clone();
            let spawned = std::thread::Builder::new()
                .name("styx-camera-client".into())
                .spawn(move || serve_client(&service, Connection::new(socket)));
            match spawned {
                Ok(handle) => clients.push(handle),
                Err(err) => tracing::warn!(error = %err, "camera client not served"),
            }
        }
    }
    for client in clients {
        let _ = client.join();
    }
}

fn serve_client(service: &Service, mut conn: Connection) {
    let Some(requirements) = handshake(&mut conn) else {
        return;
    };
    let (id, plan, frames) = match service.join(requirements) {
        Ok(joined) => joined,
        Err(reason) => {
            service.counters.rejected.fetch_add(1, Ordering::Relaxed);
            let _ = conn.send(&wire::encode_reject(&reason));
            return;
        }
    };
    if conn.send(&wire::encode_accept(&plan)).is_ok() {
        send_frames(service, &mut conn, id, &frames);
    }
    service.leave(id);
}

fn handshake(conn: &mut Connection) -> Option<FrameRequirements> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    while Instant::now() < deadline {
        let messages = conn.poll(Duration::from_millis(100)).ok()?;
        for message in messages {
            if let ClientMessage::Request(requirements) = message {
                return Some(*requirements);
            }
        }
    }
    None
}

/// Send frames while the client reads them: a client holding `max_in_flight` frames is not
/// pulled for more, so a client that stops reading lets the camera idle.
fn send_frames(service: &Service, conn: &mut Connection, id: u64, frames: &FramesSlot) {
    let max_in_flight = service.config.max_in_flight;
    let counters = &service.counters;
    while !service.stopping.load(Ordering::Acquire) {
        let wait = if conn.in_flight() >= max_in_flight {
            Duration::from_millis(20)
        } else {
            Duration::ZERO
        };
        match conn.poll(wait) {
            Ok(messages) => {
                for message in messages {
                    if let ClientMessage::Roi(roi) = message {
                        service.set_roi(id, roi, frames);
                    }
                }
            }
            Err(()) => return,
        }
        if conn.in_flight() >= max_in_flight {
            continue;
        }
        let outcome = frames
            .lock()
            .as_mut()
            .map(|frames| frames.next_frame(Duration::from_millis(50)));
        let frame = match outcome {
            Some(RecvOutcome::Data(frame)) => frame,
            Some(_) => continue,
            // The capture is restarting.
            None => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        let exported = match connection::export(&frame) {
            Ok(exported) => exported,
            Err(err) => {
                tracing::warn!(error = %err, "frame not sent");
                continue;
            }
        };
        drop(frame);
        match conn.send_frame(&exported) {
            Ok(true) => {
                counters.sent.fetch_add(1, Ordering::Relaxed);
                if exported.copied {
                    counters.copied.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(false) => {
                counters.skipped.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => return,
        }
    }
}
