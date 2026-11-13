//! The service: the scan that lists cameras and starts their workers, the workers, and the
//! control socket, run together until [`Service::shutdown`] (or drop).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU32;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use styx_native_run::Backoff;
use styx_select::listing::{LibcameraLister, ListError, Listed, NativeLister, V4l2Lister};
use styx_select::{Backend, Config as SelectConfig};
use styx_socket::{HANDSHAKE_TIMEOUT, Limits};

use crate::config::{CameraConfig, Config, ConfigError};
use crate::outlet::Outlets;
use crate::slot::{CameraStatus, Command, Publish, Slot};

/// The three backends' listers. The binary passes the real ones; tests pass simulated ones.
pub struct Listers {
    pub native: Arc<dyn NativeLister + Send + Sync>,
    pub libcamera: Arc<dyn LibcameraLister + Send + Sync>,
    pub v4l2: Arc<dyn V4l2Lister + Send + Sync>,
}

impl fmt::Debug for Listers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Listers").finish_non_exhaustive()
    }
}

/// What a service runs on.
pub struct Deps {
    /// Time for the backoff waits. The real one waits; a test one records the waits.
    pub clock: Arc<dyn styx_native_run::Clock + Send + Sync>,
    pub listers: Listers,
    pub outlets: Arc<dyn Outlets>,
}

impl fmt::Debug for Deps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deps")
            .field("listers", &self.listers)
            .finish_non_exhaustive()
    }
}

/// State every thread of the service reads.
pub(crate) struct Shared {
    pub(crate) shutdown: AtomicBool,
    pub(crate) clock: Arc<dyn styx_native_run::Clock + Send + Sync>,
    pub(crate) backoff: Backoff,
    pub(crate) auto_off: Option<NonZeroU32>,
    pub(crate) lemnos_socket: Option<PathBuf>,
    pub(crate) outlets: Arc<dyn Outlets>,
    pub(crate) config: Config,
    pub(crate) slots: Mutex<BTreeMap<String, Arc<Slot>>>,
    /// The last scan's errors, one line for the status. Empty when the scan worked.
    pub(crate) scan_errors: Mutex<String>,
    pub(crate) scan_wake: Mutex<()>,
    pub(crate) scan_cv: Condvar,
    pub(crate) workers: Mutex<Vec<JoinHandle<()>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why the service could not start.
#[derive(Debug)]
pub enum Error {
    Config(ConfigError),
    /// The control socket could not be made.
    Socket(std::io::Error),
    /// The socket directory or a thread could not be made.
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(e) => write!(f, "{e}"),
            Error::Socket(e) => write!(f, "control socket: {e}"),
            Error::Io(e) => write!(f, "service: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// A running service. Dropping it shuts it down.
pub struct Service {
    shared: Arc<Shared>,
    socket: PathBuf,
    supervisor: Option<JoinHandle<()>>,
    server: Option<JoinHandle<()>>,
}

impl fmt::Debug for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Service")
            .field("control_socket", &self.socket)
            .finish_non_exhaustive()
    }
}

impl Service {
    /// Starts the service: checks the config, binds the control socket, and starts the scan (the
    /// first scan runs at once) and the control server. Cameras named in the config get a worker
    /// at once, even before any backend lists them.
    pub fn start(config: Config, deps: Deps) -> Result<Self, Error> {
        config.check().map_err(Error::Config)?;
        let backoff = config
            .backoff()
            .map_err(|e| Error::Config(ConfigError::Backoff(e)))?;
        std::fs::create_dir_all(&config.socket_dir).map_err(Error::Io)?;
        let socket = config.control_socket();
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).map_err(Error::Socket)?;
        listener.set_nonblocking(true).map_err(Error::Socket)?;

        let shared = Arc::new(Shared {
            shutdown: AtomicBool::new(false),
            clock: deps.clock,
            backoff,
            auto_off: config.auto_off_after,
            lemnos_socket: config.lemnos_socket.clone(),
            outlets: deps.outlets,
            config: config.clone(),
            slots: Mutex::new(BTreeMap::new()),
            scan_errors: Mutex::new(String::new()),
            scan_wake: Mutex::new(()),
            scan_cv: Condvar::new(),
            workers: Mutex::new(Vec::new()),
        });
        for id in &config.cameras {
            ensure_slot(&shared, id).map_err(Error::Io)?;
        }

        let scan_shared = Arc::clone(&shared);
        let listers = deps.listers;
        let supervisor = std::thread::Builder::new()
            .name("styx-scan".to_string())
            .spawn(move || supervise(&scan_shared, &listers))
            .map_err(Error::Io)?;
        let server_shared = Arc::clone(&shared);
        let server = std::thread::Builder::new()
            .name("styx-control".to_string())
            .spawn(move || crate::control::serve(&listener, &server_shared))
            .map_err(Error::Io)?;
        Ok(Self {
            shared,
            socket,
            supervisor: Some(supervisor),
            server: Some(server),
        })
    }

    /// The control socket's path.
    pub fn control_socket(&self) -> &Path {
        &self.socket
    }

    /// Every camera the service knows, sorted by id.
    pub fn statuses(&self) -> Vec<CameraStatus> {
        lock(&self.shared.slots)
            .values()
            .map(|s| s.snapshot())
            .collect()
    }

    /// The last scan's errors, empty when the scan worked.
    pub fn scan_errors(&self) -> String {
        lock(&self.shared.scan_errors).clone()
    }

    /// Stops every thread and returns once they have ended. Cameras are stopped as their threads
    /// end; their devices are released.
    pub fn shutdown(mut self) {
        self.stop_threads();
    }

    fn stop_threads(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.scan_cv.notify_all();
        for slot in lock(&self.shared.slots).values() {
            slot.wake.store(true, Ordering::Release);
        }
        if let Some(handle) = self.supervisor.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.server.take() {
            let _ = handle.join();
        }
        let workers: Vec<_> = lock(&self.shared.workers).drain(..).collect();
        for handle in workers {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop_threads();
    }
}

/// The scan: list, choose, start workers, and wait for the next scan.
fn supervise(shared: &Arc<Shared>, listers: &Listers) {
    while !shared.shutdown.load(Ordering::Acquire) {
        scan(shared, listers);
        let guard = lock(&shared.scan_wake);
        let _ = shared
            .scan_cv
            .wait_timeout(guard, Duration::from_millis(shared.config.scan_ms.max(1)));
    }
}

/// Lists every backend and applies the chooser's plan to the slots.
fn scan(shared: &Arc<Shared>, listers: &Listers) {
    let mut errors: Vec<String> = Vec::new();
    let mut failed = [false; 3];
    let native = list(&mut errors, &mut failed[0], || listers.native.list_native());
    let libcamera = list(&mut errors, &mut failed[1], || {
        listers.libcamera.list_libcamera()
    });
    let v4l2 = list(&mut errors, &mut failed[2], || listers.v4l2.list_v4l2());

    let present = |name: &str| {
        native
            .iter()
            .chain(&libcamera)
            .chain(&v4l2)
            .any(|item| item.anchors.iter().any(|a| a.name() == name))
    };
    let config = &shared.config;
    let select_config = SelectConfig {
        force: config.force.map(|f| f.backend()),
        per_camera: config
            .camera
            .iter()
            .filter(|(name, _)| present(name))
            .filter_map(|(name, c)| c.force.map(|f| (name.clone(), f.backend())))
            .collect(),
        virtual_nodes: config
            .virtual_nodes
            .iter()
            .filter(|name| present(name))
            .cloned()
            .collect(),
    };
    match styx_select::plan(native, libcamera, v4l2, &select_config) {
        Ok(plan) => {
            let mut listed: BTreeSet<String> = BTreeSet::new();
            for selected in &plan.cameras {
                if !config.cameras.is_empty() && !config.cameras.contains(&selected.id) {
                    continue;
                }
                if ensure_slot(shared, &selected.id).is_err() {
                    errors.push(format!("camera {}: no worker thread", selected.id));
                    continue;
                }
                if let Some(slot) = lock(&shared.slots).get(&selected.id).cloned() {
                    slot.set_selection(Some(selected));
                }
                listed.insert(selected.id.clone());
            }
            // A camera no scan lists is absent, unless its backend could not list at all this
            // time: then the last selection stands, and the attempt reports the backend's error.
            for slot in lock(&shared.slots).values() {
                if listed.contains(&slot.id) {
                    continue;
                }
                let keep = slot
                    .selected()
                    .is_some_and(|s| failed[backend_index(s.owner)]);
                if !keep {
                    slot.set_selection(None);
                }
            }
        }
        Err(e) => errors.push(e.to_string()),
    }
    *lock(&shared.scan_errors) = errors.join("; ");
}

fn backend_index(backend: Backend) -> usize {
    match backend {
        Backend::Native => 0,
        Backend::Libcamera => 1,
        Backend::V4l2 => 2,
    }
}

fn list(
    errors: &mut Vec<String>,
    failed: &mut bool,
    f: impl FnOnce() -> Result<Vec<Listed>, ListError>,
) -> Vec<Listed> {
    match f() {
        Ok(items) => items,
        Err(e) => {
            errors.push(e.to_string());
            *failed = true;
            Vec::new()
        }
    }
}

/// The slot for `id`, made with its worker thread the first time.
fn ensure_slot(shared: &Arc<Shared>, id: &str) -> std::io::Result<Arc<Slot>> {
    let mut slots = lock(&shared.slots);
    if let Some(slot) = slots.get(id) {
        return Ok(Arc::clone(slot));
    }
    let (tx, rx): (mpsc::Sender<Command>, Receiver<Command>) = mpsc::channel();
    let settings: CameraConfig = shared.config.camera.get(id).cloned().unwrap_or_default();
    let slot = Arc::new(Slot::new(id.to_string(), settings, tx));
    let socket_path = camera_socket(&shared.config.socket_dir, id);
    let socket_slot = Arc::clone(&slot);
    let socket_shared = Arc::clone(shared);
    let socket_tx = slot.commands.clone();
    let socket_handle = std::thread::Builder::new()
        .name(format!("styx-socket:{id}"))
        .spawn(move || {
            serve_camera_socket(&socket_path, &socket_slot, &socket_tx, &socket_shared);
        })?;
    let worker_slot = Arc::clone(&slot);
    let worker_shared = Arc::clone(shared);
    let handle = std::thread::Builder::new()
        .name(format!("styx-camera:{id}"))
        .spawn(move || crate::worker::run(&worker_slot, &rx, &worker_shared))?;
    let mut workers = lock(&shared.workers);
    workers.push(socket_handle);
    workers.push(handle);
    drop(workers);
    slots.insert(id.to_string(), Arc::clone(&slot));
    Ok(slot)
}

/// A camera's socket for readers in other processes: `<socket_dir>/cameras/<name>-<hash>.sock`.
/// The name is the camera's id with every byte that is not a letter, digit, `-` or `_` made `_`
/// (and cut to 40 bytes); the hash is FNV-1a of the whole id, so two ids never share a path. It
/// depends only on the id, so a camera that leaves and returns keeps its path.
pub fn camera_socket(socket_dir: &Path, id: &str) -> PathBuf {
    let mut name: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    name.truncate(40);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    socket_dir
        .join("cameras")
        .join(format!("{name}-{hash:016x}.sock"))
}

/// Accepts readers on a camera's socket until shutdown. Each reader's handshake runs here, on the
/// socket's thread, never on the camera's: the camera's worker only makes the plan and attaches
/// the finished reader, both quick. A reader that does not answer within the handshake timeout is
/// dropped; a reader that connects while the camera is not publishing is closed, and connects
/// again. One handshake runs at a time per camera, so a stuck reader delays only the readers
/// behind it on this camera's socket.
fn serve_camera_socket(path: &Path, slot: &Slot, tx: &mpsc::Sender<Command>, shared: &Shared) {
    if let Some(dir) = path.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        slot.set_socket(
            None,
            Publish::NotPublishable(format!("socket directory: {e}")),
        );
        return;
    }
    let _ = std::fs::remove_file(path);
    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        Err(e) => {
            slot.set_socket(
                None,
                Publish::NotPublishable(format!("socket {}: {e}", path.display())),
            );
            return;
        }
    };
    if listener.set_nonblocking(true).is_err() {
        slot.set_socket(
            None,
            Publish::NotPublishable("socket: cannot poll".to_string()),
        );
        return;
    }
    slot.set_socket(Some(path.to_path_buf()), Publish::Waiting);
    while !shared.shutdown.load(Ordering::Acquire) {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        if stream.set_nonblocking(false).is_err() {
            continue;
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        if tx.send(Command::Handshake(reply_tx)).is_err() {
            break;
        }
        let Ok(Some((planned, plan))) = reply_rx.recv_timeout(Duration::from_secs(2)) else {
            continue;
        };
        if let Ok(accepted) = plan.run(stream, READER_LIMITS, HANDSHAKE_TIMEOUT)
            && tx.send(Command::Attach(planned, accepted)).is_err()
        {
            break;
        }
    }
    let _ = std::fs::remove_file(path);
}

/// What each reader may hold of a camera's buffers at once. A reader over this is cut off by the
/// producer (it reconnects). No time limit.
const READER_LIMITS: Limits = Limits {
    max_held: 4,
    max_hold: None,
};

/// The Lemnos daemon as a native lister: connects, lists, and disconnects on each scan.
#[derive(Debug, Clone)]
pub struct Daemon {
    pub socket: PathBuf,
}

impl NativeLister for Daemon {
    fn list_native(&self) -> Result<Vec<Listed>, ListError> {
        let client = styx_lemnos::Client::connect(&self.socket, crate::worker::CLIENT_NAME)
            .map_err(|e| ListError::new(Backend::Native, format!("lemnos: {e}")))?;
        styx_select::real::Lemnos { client: &client }.list_native()
    }
}

/// A lister that lists nothing (no daemon configured, or a backend not wanted).
#[derive(Debug, Clone, Copy, Default)]
pub struct Nothing;

impl NativeLister for Nothing {
    fn list_native(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

impl LibcameraLister for Nothing {
    fn list_libcamera(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

impl V4l2Lister for Nothing {
    fn list_v4l2(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}
