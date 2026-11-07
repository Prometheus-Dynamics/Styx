//! What the daemon serves and how: read from JSON by the binary, or built by the embedding
//! product. Every field has a default, so `{}` is a whole configuration.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use styx::ProbedDevice;
use styx::capture_api::StyxConfig;

/// The daemon's configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where the cameras' sockets go (one per camera, named after its identity key).
    pub socket_dir: PathBuf,
    /// Permissions of each socket (`0o660`: the owner and its group). `None`: the umask's.
    pub socket_mode: Option<u32>,
    /// The endpoint list, replaced atomically whenever it changes. `None`: no file.
    pub endpoints_file: Option<PathBuf>,
    /// Serve only the cameras that `cameras` names (`false`: every camera, `cameras` adjusting
    /// the ones it names).
    pub listed_only: bool,
    /// Pause a camera this long after its last client leaves (configured, not streaming).
    /// `None`: keep streaming.
    pub idle_pause_ms: Option<u64>,
    /// Capture settings of every camera (native ISP, buffers, queues), before `cameras`'
    /// overrides.
    pub capture: StyxConfig,
    /// Clients each camera's capture keeps room for beyond the first, so they join without a
    /// restart (`CameraService::spare_clients`; each spare costs a set of buffers). `None`:
    /// the service's default.
    pub spare_clients: Option<usize>,
    /// CPUs the threads of every camera's service run on (capture, ISP, 3A, clients'
    /// connections), e.g. all but the vision cores. `None`: wherever the host's thread may.
    pub cpus: Option<Vec<usize>>,
    /// How often the camera list is checked when the kernel's device events are not available.
    pub rescan_ms: u64,
    /// Settings for cameras matched by identity key or name.
    pub cameras: Vec<CameraConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            socket_dir: PathBuf::from("/run/styx/camerad"),
            socket_mode: Some(0o660),
            endpoints_file: Some(PathBuf::from("/run/styx/camerad/endpoints.json")),
            listed_only: false,
            idle_pause_ms: Some(2_000),
            capture: StyxConfig::default(),
            spare_clients: None,
            cpus: None,
            rescan_ms: 1_000,
            cameras: Vec::new(),
        }
    }
}

/// One camera's settings, over [`Config`]'s.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraConfig {
    /// An identity key of the camera (exactly), or part of its name.
    #[serde(rename = "match")]
    pub matches: String,
    /// Leave this camera alone.
    pub skip: bool,
    /// Its capture settings, in place of [`Config::capture`].
    pub capture: Option<StyxConfig>,
    /// Buffers of each ISP output it starts with (`NativeIspConfig::output_buffers`).
    pub output_buffers: Option<u32>,
    /// Most clients at once.
    pub max_clients: Option<usize>,
    /// Its spare clients, in place of [`Config::spare_clients`].
    pub spare_clients: Option<usize>,
    /// CPUs of its service's threads, in place of [`Config::cpus`].
    pub cpus: Option<Vec<usize>>,
}

impl CameraConfig {
    /// Whether this entry names `device`.
    pub fn names(&self, device: &ProbedDevice) -> bool {
        !self.matches.is_empty()
            && (device.identity.keys.contains(&self.matches)
                || device.identity.display.contains(&self.matches))
    }
}

/// The settings one camera's service gets.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub capture: StyxConfig,
    pub max_clients: Option<usize>,
    pub spare_clients: Option<usize>,
    pub cpus: Option<Vec<usize>>,
}

impl Config {
    /// What `device` is served with, or `None` when it is not served.
    pub fn resolve(&self, device: &ProbedDevice) -> Option<Resolved> {
        let entry = self.cameras.iter().find(|c| c.names(device));
        match entry {
            Some(c) if c.skip => return None,
            None if self.listed_only => return None,
            _ => {}
        }
        let mut capture = entry
            .and_then(|c| c.capture.clone())
            .unwrap_or_else(|| self.capture.clone());
        if let Some(n) = entry.and_then(|c| c.output_buffers) {
            capture = capture.native_output_buffers(n);
        }
        Some(Resolved {
            capture,
            max_clients: entry.and_then(|c| c.max_clients),
            spare_clients: entry.and_then(|c| c.spare_clients).or(self.spare_clients),
            cpus: entry
                .and_then(|c| c.cpus.clone())
                .or_else(|| self.cpus.clone()),
        })
    }
}
