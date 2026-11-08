//! The daemon: one [`CameraService`] per served camera, started when the camera appears and
//! stopped when it goes, and the endpoint list published on every change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use styx::ProbedDevice;
use styx::ipc::{CameraService, CameraServiceHandle};

use crate::config::{Config, Resolved};
use crate::endpoints::{Endpoint, EndpointSink, TRANSPORT};

/// A camera being served.
struct Running {
    /// Dropped to stop the service (its clients disconnected, its socket removed).
    _handle: CameraServiceHandle,
    endpoint: Endpoint,
    /// The device's backends as probed: a camera that comes back on other nodes restarts.
    fingerprint: String,
}

/// A camera that could not be served, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartError {
    pub camera: String,
    pub error: String,
}

/// The daemon's state: the cameras served and where their endpoints went.
pub struct Daemon<S: EndpointSink> {
    config: Config,
    sink: S,
    running: BTreeMap<String, Running>,
    published: Option<Vec<Endpoint>>,
}

impl<S: EndpointSink> Daemon<S> {
    pub fn new(config: Config, sink: S) -> Self {
        Self {
            config,
            sink,
            running: BTreeMap::new(),
            published: None,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The endpoints served now, in socket order.
    pub fn endpoints(&self) -> Vec<Endpoint> {
        self.running.values().map(|r| r.endpoint.clone()).collect()
    }

    /// Serve exactly the cameras of `devices` the configuration takes: start the new ones,
    /// restart ones whose device changed, stop the ones gone. Publishes the endpoints when they
    /// changed. Returns the cameras that failed to start (tried again on the next sync).
    pub fn sync(&mut self, devices: &[ProbedDevice]) -> Vec<StartError> {
        let mut wanted: BTreeMap<String, (&ProbedDevice, Resolved)> = BTreeMap::new();
        for d in devices {
            let Some(resolved) = self.config.resolve(d) else {
                continue;
            };
            // The first key that no camera before took (else the first, numbered).
            let stems = (0..d.identity.keys.len().max(1)).map(|k| stem_of(d, k));
            let stem = stems
                .clone()
                .find(|s| !wanted.contains_key(s))
                .unwrap_or_else(|| {
                    let first = stem_of(d, 0);
                    (2..)
                        .map(|n| format!("{first}-{n}"))
                        .find(|s| !wanted.contains_key(s))
                        .expect("a free number")
                });
            wanted.insert(stem, (d, resolved));
        }
        self.running.retain(|stem, r| {
            wanted
                .get(stem)
                .is_some_and(|(d, _)| fingerprint(d) == r.fingerprint)
        });
        let mut errors = Vec::new();
        for (stem, (device, resolved)) in wanted {
            if self.running.contains_key(&stem) {
                continue;
            }
            let path = self.config.socket_dir.join(format!("{stem}.sock"));
            match self.start(device, resolved, &path) {
                Ok(handle) => {
                    let endpoint = Endpoint {
                        name: device.identity.display.clone(),
                        product: device.identity.product.clone(),
                        keys: device.identity.keys.clone(),
                        backend: device.backends.first().map(|b| b.kind),
                        node: device.backends.first().and_then(|b| node(&b.handle)),
                        socket: path,
                        transport: TRANSPORT,
                    };
                    self.running.insert(
                        stem,
                        Running {
                            _handle: handle,
                            endpoint,
                            fingerprint: fingerprint(device),
                        },
                    );
                }
                Err(error) => errors.push(StartError {
                    camera: device.identity.display.clone(),
                    error,
                }),
            }
        }
        self.publish();
        errors
    }

    fn publish(&mut self) {
        let now = self.endpoints();
        if self.published.as_ref() != Some(&now) {
            self.sink.publish(&now);
            self.published = Some(now);
        }
    }

    fn start(
        &self,
        device: &ProbedDevice,
        resolved: Resolved,
        path: &Path,
    ) -> Result<CameraServiceHandle, String> {
        let mut service = CameraService::new(device.clone()).capture_config(resolved.capture);
        service = match self.config.idle_pause_ms {
            Some(ms) => service.pause_when_idle(Duration::from_millis(ms)),
            None => service.keep_streaming(),
        };
        if let Some(mode) = self.config.socket_mode {
            service = service.socket_mode(mode);
        }
        if let Some(n) = resolved.max_clients {
            service = service.max_clients(n);
        }
        if let Some(n) = resolved.spare_clients {
            service = service.spare_clients(n);
        }
        std::fs::create_dir_all(&self.config.socket_dir)
            .map_err(|e| format!("{}: {e}", self.config.socket_dir.display()))?;
        let path = path.to_path_buf();
        match resolved.cpus {
            None => service.serve(&path).map_err(|e| e.to_string()),
            // The service's threads descend from this one, so they keep its CPUs.
            Some(cpus) => std::thread::Builder::new()
                .name("styx-camerad-start".into())
                .spawn(move || {
                    crate::affinity::pin_current_thread(&cpus)
                        .map_err(|e| format!("CPUs {cpus:?}: {e}"))?;
                    service.serve(&path).map_err(|e| e.to_string())
                })
                .map_err(|e| e.to_string())?
                .join()
                .map_err(|_| "the start thread panicked".to_string())?,
        }
    }

    /// Stop every camera and publish the empty list.
    pub fn stop_all(&mut self) {
        self.running.clear();
        self.publish();
    }

    /// Serve the cameras attached now and as they come and go until `stop` is set: kernel
    /// device events where the platform has them, else a rescan every
    /// [`Config::rescan_ms`]. Start failures are logged and tried again on the next change,
    /// and without one after a wait doubling from 1 s up to 10 s, for as long as they fail.
    pub fn run(&mut self, stop: &AtomicBool) -> Result<(), styx::watch::WatchError> {
        let mut runtime = styx::watch::WatchRuntime::new();
        runtime.refresh();
        let mut retry = Retry::default();
        retry.after(self.sync(runtime.devices()));
        let mut watcher = styx::watch::LinuxVideoFsWatcher::new()?;
        let tick = if watcher.uses_uevents() {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(self.config.rescan_ms.max(100))
        };
        while !stop.load(Ordering::Acquire) {
            match runtime.poll_watcher_and_refresh(&mut watcher) {
                Ok(Some(_)) => retry.after(self.sync(runtime.devices())),
                Ok(None) if retry.due() => {
                    runtime.refresh();
                    retry.after(self.sync(runtime.devices()));
                }
                Ok(None) => {}
                Err(e) => eprintln!("styx-camerad: device watch: {e}"),
            }
            std::thread::sleep(tick);
        }
        self.stop_all();
        Ok(())
    }
}

fn log(errors: Vec<StartError>) {
    for e in errors {
        eprintln!("styx-camerad: {}: {}", e.camera, e.error);
    }
}

/// When to try the cameras that failed to start again: after a wait doubling from 1 s up to
/// 10 s while starts keep failing, never giving up; none pending once every camera runs.
#[derive(Default)]
struct Retry {
    next: Option<(std::time::Instant, Duration)>,
}

impl Retry {
    const FIRST: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_secs(10);

    /// Logs a sync's failures and schedules the next try if there were any.
    fn after(&mut self, errors: Vec<StartError>) {
        self.next = (!errors.is_empty()).then(|| {
            let wait = self
                .next
                .map_or(Self::FIRST, |(_, w)| (w * 2).min(Self::MAX));
            (std::time::Instant::now() + wait, wait)
        });
        log(errors);
    }

    fn due(&self) -> bool {
        self.next
            .is_some_and(|(at, _)| std::time::Instant::now() >= at)
    }
}

/// The socket's file name (without `.sock`): the camera's first identity key (else its name),
/// with anything but letters, digits, `.`, `_` and `-` as `_`. When cameras served together
/// share a first key, the later ones take their next key (or a number).
pub fn socket_stem(device: &ProbedDevice) -> String {
    stem_of(device, 0)
}

/// [`socket_stem`] from the camera's key `k`.
fn stem_of(device: &ProbedDevice, k: usize) -> String {
    let id = device
        .identity
        .keys
        .get(k)
        .unwrap_or(&device.identity.display);
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Where the backend opens the camera: its V4L2 device node, native or UVC key, or libcamera
/// id.
fn node(handle: &styx::BackendHandle) -> Option<String> {
    match handle {
        #[cfg(feature = "v4l2")]
        styx::BackendHandle::V4l2 { path } => Some(path.clone()),
        #[cfg(feature = "native")]
        styx::BackendHandle::Native { key } => Some(key.clone()),
        #[cfg(feature = "uvc")]
        styx::BackendHandle::Uvc { key } => Some(key.clone()),
        #[cfg(feature = "libcamera")]
        styx::BackendHandle::Libcamera { id } => Some(id.clone()),
        _ => None,
    }
}

fn fingerprint(device: &ProbedDevice) -> String {
    format!(
        "{:?}",
        device
            .backends
            .iter()
            .map(|b| (b.kind, &b.handle))
            .collect::<Vec<_>>()
    )
}

/// Where a camera's socket goes under `dir` (unless another camera served with it shares its
/// first key; [`Daemon::endpoints`] has the paths in use).
pub fn socket_path(dir: &Path, device: &ProbedDevice) -> PathBuf {
    dir.join(format!("{}.sock", socket_stem(device)))
}
