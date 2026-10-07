//! Where frames come from: a Styx camera service (a normal client next to the others) or the
//! camera opened directly (no service on the device).

use std::path::PathBuf;
use std::time::Duration;

use styx::ipc::{ClientOptions, ControlClient, FrameClient, IpcError, StandardControl};
use styx::prelude::*;

use crate::config::{Config, Mode};

/// Frames queued for the recorder in every-frame mode (the camera service allows 8 at most).
pub const EVERY_FRAME_QUEUE: usize = 8;

/// A camera control and its value now.
#[derive(Clone, Debug, PartialEq)]
pub struct Control {
    pub id: u32,
    pub name: String,
    /// `None`: it cannot be read.
    pub value: Option<ControlValue>,
    pub standard: Option<StandardControl>,
}

/// What a source says about itself, for the settings file.
#[derive(Clone, Debug, Default)]
pub struct SourceInfo {
    /// `service` or `direct`.
    pub kind: &'static str,
    pub socket: Option<PathBuf>,
    pub camera: String,
    pub camera_keys: Vec<String>,
    pub backend: Option<String>,
    /// The planner's plan for the recorder's frames.
    pub plan: Option<String>,
    /// The frame rate the capture runs at, when known.
    pub fps: Option<f32>,
    pub size: Option<(u32, u32)>,
    pub format: Option<String>,
}

// A frame is moved straight out of it on every call; boxing it would allocate per frame.
#[allow(clippy::large_enum_variant)]
pub enum Next {
    Frame(FrameLease),
    Empty,
    /// No more frames, and why.
    Closed(String),
}

pub trait Source {
    fn info(&self) -> &SourceInfo;
    fn next(&mut self, wait: Duration) -> Next;
    /// The camera's controls and their values now.
    fn controls(&mut self) -> Result<Vec<Control>, String>;
    /// Captures of other clients the recorder's joining restarted (service only; `None`:
    /// unknown).
    fn restarts_caused(&self) -> Option<u64> {
        None
    }
}

fn request(cfg: &Config) -> FrameRequest {
    let mut req = Frames::gray();
    if let Some((w, h)) = cfg.size {
        req = req.size(w, h);
    }
    if let Some(fps) = cfg.fps {
        req = req.fps(fps);
    }
    match cfg.mode {
        Mode::Every if cfg.stills.is_none() => req.every_frame(EVERY_FRAME_QUEUE),
        _ => req.latest(),
    }
}

/// Whether `name` (a `--camera` argument) picks a camera with this name and these keys: part of
/// its name or one of its keys, as the service matches it.
fn matches(name: &str, camera: &str, keys: &[String]) -> bool {
    camera.contains(name) || keys.iter().any(|k| k == name)
}

/// A client of a Styx camera service.
pub struct ServiceSource {
    client: FrameClient,
    controls: Option<ControlClient>,
    info: SourceInfo,
    restarts_before: Option<u64>,
    restarts_caused: Option<u64>,
    checked_restarts: bool,
}

/// The service's capture restarts so far (its Prometheus metrics, `event="restarts"`).
fn service_restarts(socket: &std::path::Path) -> Option<u64> {
    let text = FrameClient::service_metrics_text(socket).ok()?;
    text.lines()
        .find(|l| l.starts_with("styx_service_events_total{") && l.contains("event=\"restarts\""))
        .and_then(|l| l.split_whitespace().last())
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v as u64)
}

impl ServiceSource {
    pub fn open(cfg: &Config) -> Result<Self, String> {
        let options = || {
            let o = FrameClient::options(&cfg.socket).timeout(Duration::from_secs(5));
            match &cfg.camera {
                Some(c) => o.camera(c.clone()),
                None => o,
            }
        };
        let cameras = options().cameras().map_err(|err| {
            format!(
                "no Styx camera service at {}: {err}. Is it running (--socket, $STYX_SOCKET)? \
                 On a device without one (e.g. PhotonVision), use --source direct",
                cfg.socket.display()
            )
        })?;
        let camera = match &cfg.camera {
            Some(name) => cameras.iter().find(|c| matches(name, &c.name, &c.keys)),
            None => cameras.first(),
        }
        .ok_or_else(|| {
            let names: Vec<&str> = cameras.iter().map(|c| c.name.as_str()).collect();
            format!(
                "the camera service has no camera {:?} (it serves: {})",
                cfg.camera.as_deref().unwrap_or(""),
                names.join(", ")
            )
        })?
        .clone();
        let restarts_before = service_restarts(&cfg.socket);
        let client = options().request(&request(cfg)).map_err(|err| match err {
            IpcError::Rejected(why) => {
                format!("the camera service cannot serve grey frames: {why}")
            }
            other => format!("requesting frames from {}: {other}", camera.name),
        })?;
        let controls = control_options(options()).controls().ok();
        let delivered = client.delivered();
        let info = SourceInfo {
            kind: "service",
            socket: Some(cfg.socket.clone()),
            camera: camera.name.clone(),
            camera_keys: camera.keys.clone(),
            backend: None,
            plan: client.plan(),
            fps: delivered.as_ref().and_then(|d| d.fps),
            size: delivered.as_ref().map(|d| d.size),
            format: delivered.as_ref().map(|d| d.format.to_string()),
        };
        Ok(Self {
            client,
            controls,
            info,
            restarts_before,
            restarts_caused: None,
            checked_restarts: false,
        })
    }
}

fn control_options(options: ClientOptions) -> ClientOptions {
    options.timeout(Duration::from_secs(1))
}

impl Source for ServiceSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn next(&mut self, wait: Duration) -> Next {
        match self.client.recv(wait) {
            RecvOutcome::Data(frame) => {
                if !self.checked_restarts {
                    // Joined and streaming: did joining restart the others' capture?
                    self.checked_restarts = true;
                    if let (Some(before), Some(socket)) = (self.restarts_before, &self.info.socket)
                    {
                        self.restarts_caused =
                            service_restarts(socket).map(|now| now.saturating_sub(before));
                    }
                }
                Next::Frame(frame)
            }
            RecvOutcome::Empty => Next::Empty,
            RecvOutcome::Closed => Next::Closed(match self.client.last_error() {
                Some(err) => format!("the camera service closed the connection: {err}"),
                None => "the camera service closed the connection".into(),
            }),
        }
    }

    fn controls(&mut self) -> Result<Vec<Control>, String> {
        let client = self.controls.as_ref().ok_or("no control connection")?;
        let list = client.controls().map_err(|e| e.to_string())?;
        Ok(list
            .into_iter()
            .map(|d| Control {
                id: d.meta.id.0,
                name: d.meta.name,
                value: d.current,
                standard: d.standard,
            })
            .collect())
    }

    fn restarts_caused(&self) -> Option<u64> {
        self.restarts_caused
    }
}

/// The camera opened by the recorder itself (no camera service).
pub struct DirectSource {
    frames: styx::planner::Frames,
    metas: Vec<ControlMeta>,
    info: SourceInfo,
}

impl DirectSource {
    /// Probe the cameras and open the one `cfg.camera` names (else the first).
    pub fn open(cfg: &Config) -> Result<Self, String> {
        let devices = styx::probe_all();
        let device = match &cfg.camera {
            Some(name) => devices
                .iter()
                .find(|d| matches(name, &d.identity.display, &d.identity.keys)),
            None => devices.first(),
        }
        .ok_or_else(|| {
            let names: Vec<&str> = devices
                .iter()
                .map(|d| d.identity.display.as_str())
                .collect();
            if names.is_empty() {
                format!(
                    "no camera found{}",
                    if cfg!(any(
                        feature = "libcamera",
                        feature = "native",
                        feature = "v4l2"
                    )) {
                        " (another process may hold it, or it is not connected)"
                    } else {
                        ": this styx-record was built without a direct backend \
                         (features libcamera, native, v4l2); use --source service"
                    }
                )
            } else {
                format!(
                    "no camera {:?} (found: {})",
                    cfg.camera.as_deref().unwrap_or(""),
                    names.join(", ")
                )
            }
        })?
        .clone();
        Self::open_device(cfg, device)
    }

    /// Open `device` for `cfg`'s frames.
    pub fn open_device(cfg: &Config, device: ProbedDevice) -> Result<Self, String> {
        let name = device.identity.display.clone();
        let frames = request(cfg)
            .open(&device)
            .map_err(|err| open_error(&name, &err))?;
        let backend = frames.capture().backend();
        let metas = device
            .backends
            .iter()
            .find(|b| b.kind == backend)
            .map(|b| b.descriptor.controls.clone())
            .unwrap_or_default();
        let delivered = frames.plan().delivered();
        let info = SourceInfo {
            kind: "direct",
            socket: None,
            camera: name,
            camera_keys: device.identity.keys.clone(),
            backend: Some(backend.to_string()),
            plan: Some(frames.plan().to_string()),
            fps: delivered
                .fps
                .or_else(|| frames.capture().interval().map(|i| i.fps())),
            size: Some(delivered.size),
            format: Some(delivered.format.to_string()),
        };
        Ok(Self {
            frames,
            metas,
            info,
        })
    }
}

/// A clear message for a camera that would not open, a busy one above all.
fn open_error(camera: &str, err: &styx::planner::OpenError) -> String {
    #[cfg(feature = "libcamera")]
    let busy = matches!(
        err,
        styx::planner::OpenError::Capture(styx::capture_api::CaptureError::LibcameraBusy(_))
    );
    #[cfg(not(feature = "libcamera"))]
    let busy = false;
    let text = err.to_string();
    if busy || text.to_ascii_lowercase().contains("busy") {
        format!(
            "camera {camera} is busy: another process has it open (PhotonVision, a Styx camera \
             service, a libcamera app). Stop it first, or record through its Styx camera \
             service (--source service). ({text})"
        )
    } else {
        format!("opening camera {camera}: {text}")
    }
}

impl Source for DirectSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn next(&mut self, wait: Duration) -> Next {
        match self.frames.next_frame(wait) {
            RecvOutcome::Data(frame) => Next::Frame(frame),
            RecvOutcome::Empty => Next::Empty,
            RecvOutcome::Closed => Next::Closed(match self.frames.capture().last_error() {
                Some(err) => format!("the capture stopped: {err}"),
                None => "the capture stopped".into(),
            }),
        }
    }

    fn controls(&mut self) -> Result<Vec<Control>, String> {
        Ok(self
            .metas
            .iter()
            .map(|m| Control {
                id: m.id.0,
                name: m.name.clone(),
                value: self.frames.get_control(m.id).ok(),
                standard: StandardControl::ALL
                    .into_iter()
                    .find(|s| s.name() == m.name),
            })
            .collect())
    }
}
