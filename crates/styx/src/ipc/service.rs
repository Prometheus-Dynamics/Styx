//! Cameras for other processes: each client asks for the frames it needs from a camera, and the
//! service plans one shared capture per camera for all of that camera's clients.

mod camera;

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_core::prelude::*;

pub(super) use self::camera::fuzz_request;
use self::camera::{Camera, FRAME_WAIT, FramesSlot, check_request};
use super::connection::{self, Connection};
use super::socket::{self, PeerCredentials};
use super::wire::{self, CameraInfo, ClientMessage};
use super::{DEFAULT_MAX_HOLD, DEFAULT_MAX_IN_FLIGHT, IpcError};
use crate::capture_api::IdleStop;
use crate::prelude::ProbedDevice;

/// How long a new client has to send its request.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Clients served at once unless [`CameraService::max_clients`] says otherwise.
pub const DEFAULT_MAX_CLIENTS: usize = 16;

type Authorize = Arc<dyn Fn(&PeerCredentials) -> bool + Send + Sync>;

/// Serves cameras to [`FrameClient`](super::FrameClient)s in other processes.
///
/// Each client names a camera (or takes the first) and sends the
/// [`FrameRequest`](crate::planner::FrameRequest) for the frames it needs
/// ([`FrameClient::request`](super::FrameClient::request)). For each camera the service plans one
/// shared capture for all its clients ([`plan_many`](crate::planner::plan_many)): hardware
/// scaling, both ISP outputs, decoding and encoding once for clients with the same needs. A
/// client joining a capture that already fits it is attached without disturbing the others; one
/// that needs another mode or ISP setup restarts that camera's capture once. Frames go out as
/// descriptors (camera buffers and memfds), so nothing is copied. A client gets frames only as
/// fast as it drops them, and a camera stops (by default: pauses) when no client reads.
///
/// Requests come from other processes, so the service checks them before planning, limits the
/// clients it serves ([`CameraService::max_clients`]) and can check who connects
/// ([`CameraService::authorize`]).
#[derive(Clone)]
pub struct CameraService {
    config: ServiceConfig,
}

#[derive(Clone)]
pub(crate) struct ServiceConfig {
    cameras: Cameras,
    idle: Option<(Duration, IdleStop)>,
    max_in_flight: usize,
    max_hold: Option<Duration>,
    max_clients: usize,
    authorize: Option<Authorize>,
    socket_mode: Option<u32>,
}

#[derive(Clone)]
enum Cameras {
    /// These cameras only.
    Fixed(Vec<ProbedDevice>),
    /// Every camera probing finds when a client asks, including cameras plugged in later.
    Probe,
}

impl CameraService {
    /// Serve one camera.
    pub fn new(device: ProbedDevice) -> Self {
        Self::with_cameras(vec![device])
    }

    /// Serve these cameras; clients choose one by name ([`CameraInfo::name`] or part of it) or
    /// identity key.
    pub fn with_cameras(devices: Vec<ProbedDevice>) -> Self {
        Self::from(Cameras::Fixed(devices))
    }

    /// Serve every camera attached now or later: cameras are probed when a client asks for one
    /// or for the list, so hot-plugged cameras appear, and a camera that goes away while in use
    /// is reconnected when it comes back.
    pub fn all_cameras() -> Self {
        Self::from(Cameras::Probe)
    }

    fn from(cameras: Cameras) -> Self {
        Self {
            config: ServiceConfig {
                cameras,
                idle: Some((Duration::from_secs(2), IdleStop::Pause)),
                max_in_flight: DEFAULT_MAX_IN_FLIGHT,
                max_hold: Some(DEFAULT_MAX_HOLD),
                max_clients: DEFAULT_MAX_CLIENTS,
                authorize: None,
                socket_mode: None,
            },
        }
    }

    /// Pause a camera after `after` without a reading client (the default, after 2 s):
    /// libcamera cameras stay configured and start again in ~0.1 s; others release.
    pub fn pause_when_idle(mut self, after: Duration) -> Self {
        self.config.idle = Some((after, IdleStop::Pause));
        self
    }

    /// Release a camera after `after` without a reading client, and when its last client leaves.
    pub fn stop_when_idle(mut self, after: Duration) -> Self {
        self.config.idle = Some((after, IdleStop::Release));
        self
    }

    /// Keep cameras streaming while clients are connected, reading or not.
    pub fn keep_streaming(mut self) -> Self {
        self.config.idle = None;
        self
    }

    /// Frames a client may hold at once (default [`DEFAULT_MAX_IN_FLIGHT`]).
    pub fn max_in_flight(mut self, frames: usize) -> Self {
        self.config.max_in_flight = frames.max(1);
        self
    }

    /// How long a client may hold a frame (default [`DEFAULT_MAX_HOLD`]; `None`: no limit). A
    /// client holding one longer is disconnected and its frames are taken back, so the camera
    /// gets its buffers again; a consumer that needs a frame for longer copies it.
    pub fn max_hold(mut self, max: Option<Duration>) -> Self {
        self.config.max_hold = max;
        self
    }

    /// Clients served at once, across all cameras (default [`DEFAULT_MAX_CLIENTS`]); more are
    /// refused.
    pub fn max_clients(mut self, clients: usize) -> Self {
        self.config.max_clients = clients.max(1);
        self
    }

    /// Serve only processes `allow` accepts, given the credentials the kernel reports for them
    /// (e.g. `|peer| peer.uid == 0`). Others are disconnected before they send anything.
    pub fn authorize(
        mut self,
        allow: impl Fn(&PeerCredentials) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.config.authorize = Some(Arc::new(allow));
        self
    }

    /// Permissions of the socket file (e.g. `0o660` for the owner and its group), which decide
    /// who may connect. By default the process umask applies.
    pub fn socket_mode(mut self, mode: u32) -> Self {
        self.config.socket_mode = Some(mode);
        self
    }

    /// Listen on the Unix socket at `path` (replacing a stale socket file there) and serve
    /// clients on background threads until the handle is stopped or dropped.
    pub fn serve(self, path: impl AsRef<Path>) -> Result<CameraServiceHandle, IpcError> {
        let path = path.as_ref().to_path_buf();
        let listener = socket::listen(&path)?;
        if let Some(mode) = self.config.socket_mode {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        }
        let service = Arc::new(Service {
            config: self.config,
            path,
            stopping: AtomicBool::new(false),
            cameras: Mutex::new(Vec::new()),
            counters: Counters::default(),
            connections: AtomicUsize::new(0),
            client_metrics: Default::default(),
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
    /// Requests refused: the camera cannot serve them, they were malformed or out of bounds, or
    /// the service was full.
    pub rejected: u64,
    /// Connections refused by [`CameraService::authorize`].
    pub unauthorized: u64,
    /// Times a capture restarted because a new client needed another setup.
    pub restarts: u64,
    /// Frames sent, counted once per client.
    pub sent: u64,
    /// Frames that had to be copied into a memfd.
    pub copied: u64,
    /// Frames a client's socket had no room for.
    pub skipped: u64,
    /// Clients disconnected for holding a frame longer than [`CameraService::max_hold`].
    pub revoked: u64,
}

/// A running [`CameraService`]; stops it when dropped.
pub struct CameraServiceHandle {
    service: Arc<Service>,
    accept: Option<JoinHandle<()>>,
}

impl CameraServiceHandle {
    pub fn stats(&self) -> CameraServiceStats {
        self.service.stats()
    }

    /// The service's counters, each client's frames and hold times, and the metrics of every
    /// capture in this process; what [`FrameClient::service_metrics`] gets from another
    /// process.
    ///
    /// [`FrameClient::service_metrics`]: super::FrameClient::service_metrics
    pub fn metrics(&self) -> crate::metrics::ServiceMetrics {
        self.service.metrics()
    }
}

impl Service {
    fn stats(&self) -> CameraServiceStats {
        let counters = &self.counters;
        CameraServiceStats {
            clients: self.clients(),
            rejected: counters.rejected.load(Ordering::Relaxed),
            unauthorized: counters.unauthorized.load(Ordering::Relaxed),
            restarts: counters.restarts.load(Ordering::Relaxed),
            sent: counters.sent.load(Ordering::Relaxed),
            copied: counters.copied.load(Ordering::Relaxed),
            skipped: counters.skipped.load(Ordering::Relaxed),
            revoked: counters.revoked.load(Ordering::Relaxed),
        }
    }

    fn metrics(&self) -> crate::metrics::ServiceMetrics {
        let stats = self.stats();
        crate::metrics::ServiceMetrics {
            clients: stats.clients,
            rejected: stats.rejected,
            unauthorized: stats.unauthorized,
            restarts: stats.restarts,
            sent: stats.sent,
            copied: stats.copied,
            skipped: stats.skipped,
            revoked: stats.revoked,
            client_metrics: self.client_metrics.snapshot(),
            snapshot: crate::metrics::snapshot(),
        }
    }
}

impl CameraServiceHandle {
    /// The shared plans running now, one per camera in use, as text.
    pub fn plan(&self) -> Option<String> {
        let plans: Vec<String> = self
            .service
            .cameras
            .lock()
            .iter()
            .filter_map(|c| c.plan())
            .collect();
        (!plans.is_empty()).then(|| plans.concat())
    }

    /// The cameras the service serves (probing for them if it serves all cameras).
    pub fn cameras(&self) -> Vec<CameraInfo> {
        self.service.list()
    }

    pub fn path(&self) -> &Path {
        &self.service.path
    }

    /// Disconnect every client and stop the cameras.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.service.stopping.store(true, Ordering::Release);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        for camera in self.service.cameras.lock().drain(..) {
            camera.shut_down();
        }
        let _ = std::fs::remove_file(&self.service.path);
    }
}

impl Drop for CameraServiceHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Default)]
pub(crate) struct Counters {
    rejected: AtomicU64,
    unauthorized: AtomicU64,
    restarts: AtomicU64,
    sent: AtomicU64,
    copied: AtomicU64,
    skipped: AtomicU64,
    revoked: AtomicU64,
}

struct Service {
    config: ServiceConfig,
    path: PathBuf,
    stopping: AtomicBool,
    /// Cameras clients have asked for.
    cameras: Mutex<Vec<Arc<Camera>>>,
    counters: Counters,
    /// Open connections, including clients still sending their request.
    connections: AtomicUsize,
    /// Each client's frames, for [`CameraServiceHandle::metrics`].
    client_metrics: super::metrics::Clients,
}

/// Whether two probes found the same camera: probes list its identity keys in any order.
fn same_camera(a: &ProbedDevice, b: &ProbedDevice) -> bool {
    fn keys(d: &ProbedDevice) -> Vec<&str> {
        let mut keys: Vec<&str> = d.identity.keys.iter().map(String::as_str).collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }
    a.identity.display == b.identity.display && keys(a) == keys(b)
}

impl Service {
    fn devices(&self) -> Vec<ProbedDevice> {
        match &self.config.cameras {
            Cameras::Fixed(devices) => devices.clone(),
            Cameras::Probe => crate::probe_all(),
        }
    }

    fn list(&self) -> Vec<CameraInfo> {
        let devices = self.devices();
        let cameras = self.cameras.lock();
        devices
            .iter()
            .map(|device| CameraInfo {
                name: device.identity.display.clone(),
                keys: device.identity.keys.clone(),
                in_use: cameras
                    .iter()
                    .any(|c| same_camera(&c.device, device) && c.clients() > 0),
            })
            .collect()
    }

    /// The camera `selector` names (its name, part of it, or an identity key), or the first.
    fn camera(&self, selector: Option<&str>) -> Result<Arc<Camera>, String> {
        let devices = self.devices();
        let device = match selector {
            None => devices.first(),
            Some(name) => devices
                .iter()
                .find(|d| d.identity.display == name || d.identity.keys.iter().any(|k| k == name))
                .or_else(|| devices.iter().find(|d| d.identity.display.contains(name))),
        }
        .ok_or_else(|| match selector {
            Some(name) => format!("no camera named {name}"),
            None => "no camera".into(),
        })?;
        let mut cameras = self.cameras.lock();
        if let Some(camera) = cameras.iter().find(|c| same_camera(&c.device, device)) {
            return Ok(camera.clone());
        }
        let camera = Arc::new(Camera::new(device.clone()));
        cameras.push(camera.clone());
        Ok(camera)
    }

    fn clients(&self) -> usize {
        self.cameras.lock().iter().map(|c| c.clients()).sum()
    }
}

fn accept_loop(service: &Arc<Service>, listener: &OwnedFd) {
    let mut clients: Vec<JoinHandle<()>> = Vec::new();
    // Connections beyond this are closed at once, so a flood of connections cannot exhaust
    // threads while handshakes are pending.
    let max_connections = service.config.max_clients + 8;
    while !service.stopping.load(Ordering::Acquire) {
        if !socket::readable(listener, Duration::from_millis(100)) {
            clients.retain(|c| !c.is_finished());
            continue;
        }
        while let Ok(Some(socket)) = socket::accept(listener) {
            if let Some(allow) = &service.config.authorize {
                let allowed = socket::peer_credentials(&socket).is_ok_and(|peer| allow(&peer));
                if !allowed {
                    service
                        .counters
                        .unauthorized
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
            if service.connections.load(Ordering::Acquire) >= max_connections {
                service.counters.rejected.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            service.connections.fetch_add(1, Ordering::AcqRel);
            let service = service.clone();
            let spawned = std::thread::Builder::new()
                .name("styx-camera-client".into())
                .spawn(move || {
                    serve_client(&service, Connection::new(socket));
                    service.connections.fetch_sub(1, Ordering::AcqRel);
                });
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
    let Some((request, selector)) = handshake(service, &mut conn) else {
        return;
    };
    let joined = check_request(&request)
        .and_then(|()| {
            if service.clients() >= service.config.max_clients {
                return Err(format!(
                    "the service already serves {} clients",
                    service.config.max_clients
                ));
            }
            service.camera(selector.as_deref())
        })
        .and_then(|camera| {
            camera
                .join(request, &service.config, &service.counters)
                .map(|joined| (camera, joined))
        });
    let (camera, (id, plan, delivered, frames)) = match joined {
        Ok(joined) => joined,
        Err(reason) => {
            service.counters.rejected.fetch_add(1, Ordering::Relaxed);
            let _ = conn.send(&wire::encode_reject(&reason));
            return;
        }
    };
    if conn.send(&wire::encode_accept(&plan, &delivered)).is_ok() {
        service
            .client_metrics
            .add(id, &camera.device.identity.display, &mut conn);
        send_frames(service, &camera, &mut conn, id, &frames);
        super::metrics::client_left(&mut conn);
    }
    camera.leave(id, &service.config);
}

/// The client's request; a camera list request is answered here (and ends the connection).
fn handshake(
    service: &Service,
    conn: &mut Connection,
) -> Option<(crate::planner::FrameRequest, Option<String>)> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    while Instant::now() < deadline && !service.stopping.load(Ordering::Acquire) {
        let messages = conn.poll(Duration::from_millis(100)).ok()?;
        for message in messages {
            match message {
                ClientMessage::Request(request, camera) => {
                    return Some((*request, camera));
                }
                ClientMessage::List => {
                    let _ = conn.send(&wire::encode_cameras(&service.list()));
                    return None;
                }
                ClientMessage::Metrics(format) => {
                    super::metrics::answer(conn, format, &service.metrics());
                    return None;
                }
                ClientMessage::Release(_) | ClientMessage::Roi(_) => {}
            }
        }
    }
    None
}

/// Send frames while the client reads them: a client holding `max_in_flight` frames is not
/// pulled for more, so a client that stops reading lets the camera idle.
fn send_frames(
    service: &Service,
    camera: &Camera,
    conn: &mut Connection,
    id: u64,
    frames: &FramesSlot,
) {
    let max_in_flight = service.config.max_in_flight;
    let counters = &service.counters;
    // An H.264/H.265 client that missed a packet cannot decode until the next keyframe.
    let mut awaiting_keyframe = false;
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
                        camera.set_roi(id, roi, frames);
                    }
                }
            }
            Err(()) => return,
        }
        if conn.overheld(service.config.max_hold) {
            counters.revoked.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                max_hold = ?service.config.max_hold,
                "camera service: disconnecting a client that held a frame too long"
            );
            return;
        }
        if conn.in_flight() >= max_in_flight {
            continue;
        }
        let outcome = frames
            .lock()
            .as_mut()
            .map(|frames| frames.next_frame(FRAME_WAIT));
        let frame = match outcome {
            Some(RecvOutcome::Data(frame)) => frame,
            Some(_) => continue,
            // The capture is restarting.
            None => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        let delta = frame.meta().delta;
        if awaiting_keyframe && delta {
            continue;
        }
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
                awaiting_keyframe = false;
                counters.sent.fetch_add(1, Ordering::Relaxed);
                if exported.copied {
                    counters.copied.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(false) => {
                counters.skipped.fetch_add(1, Ordering::Relaxed);
                if let Some(stats) = &conn.stats {
                    stats.dropped();
                }
                if let Some(frames) = frames.lock().as_ref()
                    && frames.plan().inter_coded()
                {
                    awaiting_keyframe = true;
                    frames.request_keyframe();
                }
            }
            Err(_) => return,
        }
    }
}
