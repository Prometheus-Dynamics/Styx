//! Where frames come from: a Styx camera service (a normal client next to the others) or the
//! camera opened directly (no service on the device).

use std::path::PathBuf;
use std::time::Duration;

use styx::capture_api::ControlPlane;
use styx::ipc::{ClientOptions, ControlClient, FrameClient, IpcError, StandardControl};
use styx::planner::StepKind;
use styx::prelude::*;

use crate::config::{Config, Mode};

/// Frames queued for the recorder in every-frame mode (the camera service allows 8 at most).
pub const EVERY_FRAME_QUEUE: usize = 8;

/// The raw 8-bit sensor formats `--raw` takes (one byte per pixel, written as they come): grey
/// (a mono sensor, or one libcamera calls mono) and 8-bit Bayer.
pub const RAW_FORMATS: [FourCc; 7] = [
    FourCc::R8,
    FourCc::GREY,
    FourCc::new(*b"BA81"),
    FourCc::new(*b"BGGR"),
    FourCc::new(*b"GBRG"),
    FourCc::new(*b"GRBG"),
    FourCc::new(*b"RGGB"),
];

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
    /// What the recorded bytes are: `NV12 Y plane via ISP`, `raw R8`, `GREY`, ...
    pub source_stream: Option<String>,
}

/// Reads the camera's controls; moved to its own thread by the recorder, so a slow read (a
/// libcamera read waits for the capture thread, up to a frame period per control) never holds
/// up frames.
pub trait ControlReader: Send {
    /// The camera's controls and their values now.
    fn read(&mut self) -> Result<Vec<Control>, String>;
}

/// No controls to read.
struct NoControls(String);

impl ControlReader for NoControls {
    fn read(&mut self) -> Result<Vec<Control>, String> {
        Err(self.0.clone())
    }
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
    /// The reader of the camera's controls (taken once; later calls may return one that fails).
    fn control_reader(&mut self) -> Box<dyn ControlReader>;
    /// Captures of other clients the recorder's joining restarted (service only; `None`:
    /// unknown).
    fn restarts_caused(&self) -> Option<u64> {
        None
    }
}

fn request(cfg: &Config) -> FrameRequest {
    // Grey is the Y plane of the ISP's processed output (a zero-copy view); the raw sensor
    // stream only when asked for.
    let mut req = if cfg.raw {
        Frames::formats(RAW_FORMATS)
    } else {
        Frames::gray()
    };
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

/// What a plan's frames are, for the settings file and the CSV docs: `<code> Y plane via ISP`
/// (the luma view of the ISP's processed output), `raw <code>` (the sensor's raw stream),
/// `<code> decoded to grey`, or the camera's own grey format.
pub fn source_stream(code: FourCc, luma_view: bool, decoded: bool, raw: bool, isp: bool) -> String {
    let via = if isp { " via ISP" } else { "" };
    if luma_view {
        format!("{code} Y plane{via}")
    } else if decoded {
        format!("{code} decoded to grey")
    } else if raw {
        format!("raw {code}")
    } else {
        format!("{code}{via}")
    }
}

/// [`source_stream`] from a printed plan (a camera service's): `frame plan for <camera> via
/// <backend> <code> <W>x<H>...`, its steps and capture detail.
pub fn source_stream_of_plan_text(plan: &str) -> Option<String> {
    let first = plan.lines().next()?;
    let after = &first[first.rfind(" via ")? + 5..];
    let mut words = after.split_whitespace().skip(1);
    let code = words.next()?;
    let code = FourCc::new(code_bytes(code)?);
    let capture = plan.lines().find(|l| l.contains(" capture "))?;
    let isp = capture.contains("ISP") && !capture.contains("not processed by the ISP");
    let raw = capture.contains("raw sensor stream") || code.is_bayer_raw();
    Some(source_stream(
        code,
        plan.contains(" luma view "),
        plan.contains(" decode "),
        raw,
        isp,
    ))
}

/// A fourcc as printed (`R8`, `NV12`): padded with spaces to four bytes.
fn code_bytes(code: &str) -> Option<[u8; 4]> {
    let b = code.as_bytes();
    if b.is_empty() || b.len() > 4 {
        return None;
    }
    let mut out = [b' '; 4];
    out[..b.len()].copy_from_slice(b);
    Some(out)
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
            source_stream: client
                .plan()
                .as_deref()
                .and_then(source_stream_of_plan_text),
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

    fn control_reader(&mut self) -> Box<dyn ControlReader> {
        match self.controls.take() {
            Some(client) => Box::new(ServiceControls(client)),
            None => Box::new(NoControls("no control connection".into())),
        }
    }

    fn restarts_caused(&self) -> Option<u64> {
        self.restarts_caused
    }
}

/// The controls of a camera service's camera, over its control connection.
struct ServiceControls(ControlClient);

impl ControlReader for ServiceControls {
    fn read(&mut self) -> Result<Vec<Control>, String> {
        let list = self.0.controls().map_err(|e| e.to_string())?;
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
}

/// The camera opened by the recorder itself (no camera service).
pub struct DirectSource {
    frames: styx::planner::Frames,
    metas: Vec<ControlMeta>,
    info: SourceInfo,
}

/// The controls of a camera the recorder opened, read through its control plane.
struct DirectControls {
    plane: ControlPlane,
    metas: Vec<ControlMeta>,
}

impl ControlReader for DirectControls {
    fn read(&mut self) -> Result<Vec<Control>, String> {
        Ok(self
            .metas
            .iter()
            .map(|m| Control {
                id: m.id.0,
                name: m.name.clone(),
                value: self.plane.get_control(m.id).ok(),
                standard: StandardControl::ALL
                    .into_iter()
                    .find(|s| s.name() == m.name),
            })
            .collect())
    }
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
        let plan = frames.plan();
        let delivered = plan.delivered();
        let has = |kind: StepKind| plan.steps.iter().any(|s| s.kind == kind);
        let source_stream = source_stream(
            plan.mode.format.code,
            has(StepKind::LumaView),
            has(StepKind::Decode),
            plan.raw_sensor_stream(),
            !plan.raw_sensor_stream()
                && plan
                    .steps
                    .iter()
                    .any(|s| s.kind == StepKind::Capture && s.detail.contains("ISP")),
        );
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
            source_stream: Some(source_stream),
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

    fn control_reader(&mut self) -> Box<dyn ControlReader> {
        Box::new(DirectControls {
            plane: self.frames.capture().control_plane(),
            metas: self.metas.clone(),
        })
    }
}
