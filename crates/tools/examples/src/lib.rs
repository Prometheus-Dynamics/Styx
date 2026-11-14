//! Shared plumbing for the examples.
//!
//! An example runs in one of two ways. With `--sim` it starts the simulated backend itself: one
//! simulated camera on `lemnosd` in this process ([`Lab::sim`]). Otherwise it reads the real
//! backends: the Lemnos daemon named by `--lemnos SOCKET` (native cameras), and libcamera and V4L2
//! through `styx-select`. Either way the camera is opened the way the service opens it: the
//! chooser picks the owner ([`styx_select::select`]), and the owner's capture is started
//! ([`with_first_camera`]).
//!
//! The simulated drivers are a copy of the service's test simulator (see [`sim`]); the examples
//! use it through the Lemnos daemon, as the service does on a board.

use styx_sim::board::{Board, Family};
use styx_sim::drivers::{SimIsp, SimReceiver, SimSensor};

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config as DaemonConfig, Service as DaemonService};
use styx_format::{FourCc, Interval};
use styx_lemnos::Client;
use styx_libcamera::CaptureConfig;
use styx_native::discover;
use styx_native_run::{Initial, Policy, Request, Session, SystemClock};
use styx_select::adapters::{LibcameraCapture, NativeCapture, V4l2Capture};
use styx_select::listing::{LibcameraLister, ListError, Listed, NativeLister, V4l2Lister};
use styx_select::real::{Libcamera, Probe};
use styx_select::{Backend, Capture, Config as SelectConfig, Selected, select};
use styx_service::camera_socket;
use styx_service::{Config as ServiceConfig, Daemon, Deps, Discard, Listers, Nothing, Service};
use styx_socket::Reader;
use styx_v4l2::StreamConfig;

/// The name the examples give their Lemnos clients and daemons.
const NAME: &str = "styx-examples";

/// The id of the simulated camera, as the chooser names it (`lemnos:` and its sensor's name).
pub const SIM_CAMERA: &str = "lemnos:cam0-sensor";

/// The frame rate asked of every camera, in thousandths per second: the simulated mode's rate.
pub const FPS_MILLI: u32 = 60_000;
/// The exposure and analogue gain a native camera starts with.
pub const EXPOSURE_US: u32 = 10_000;
pub const GAIN_MILLI: u32 = 1_000;

/// The pixel format and size asked of a V4L2 or libcamera camera. The examples do not pick a
/// format per camera; a camera that does not take this one fails to open and says why.
const FALLBACK_CODE: FourCc = FourCc::YUYV;
const FALLBACK_SIZE: (u32, u32) = (640, 480);

/// Where the examples' cameras come from: the simulated backend, or the real ones.
pub struct Lab {
    /// The Lemnos daemon's socket: the simulated one, or the real one named on the command line.
    socket: Option<PathBuf>,
    /// The simulated backend, when there is one. Its daemon stops when the lab is dropped.
    sim: Option<SimHost>,
}

impl std::fmt::Debug for Lab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lab")
            .field("socket", &self.socket)
            .field("sim", &self.sim.is_some())
            .finish()
    }
}

/// The simulated backend: the daemon, then the directory of its socket. The daemon is declared
/// first, so it stops before its directory goes.
struct SimHost {
    _daemon: DaemonService,
    _dir: DirGuard,
}

/// Removes a directory when dropped.
struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Lab {
    /// The real backends. `lemnos` is the Lemnos daemon's socket, when there is one (native
    /// cameras are listed only with it); libcamera and V4L2 are listed through `styx-select`.
    pub fn real(lemnos: Option<PathBuf>) -> Self {
        Self {
            socket: lemnos,
            sim: None,
        }
    }

    /// The simulated backend: `lemnosd` in this process, with one simulated camera
    /// ([`SIM_CAMERA`]). `name` keeps this process's socket directory apart from others.
    pub fn sim(name: &str) -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!("styx-examples-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let guard = DirGuard(dir.clone());
        let socket = dir.join("lemnos.sock");
        let board = parse(&Board::timed(Family::Run).decoy(false).text())
            .map_err(|e| format!("board: {e:?}"))?;
        let config = DaemonConfig {
            socket: socket.clone(),
            name: NAME.to_string(),
            board,
            drivers: vec![
                entry::<SimSensor>(),
                entry::<SimReceiver>(),
                entry::<SimIsp>(),
            ],
            backend: Box::new(SimBoard::new()),
            options: Options {
                call_deadline: Duration::from_millis(200),
                ..Options::default()
            },
        };
        let daemon = DaemonService::start(config).map_err(|e| format!("lemnosd: {e:?}"))?;
        Ok(Self {
            socket: Some(socket),
            sim: Some(SimHost {
                _daemon: daemon,
                _dir: guard,
            }),
        })
    }

    /// Whether this lab is the simulated backend.
    pub fn is_sim(&self) -> bool {
        self.sim.is_some()
    }

    /// The Lemnos daemon's socket, if the lab has one.
    pub fn lemnos_socket(&self) -> Option<&Path> {
        self.socket.as_deref()
    }
}

/// Opens the first camera the chooser plans and runs `run` on its capture and its selection. The
/// capture stops when `run` returns.
///
/// With the simulated backend only the simulated camera is listed: libcamera and V4L2 are not
/// probed, so the run does not depend on the machine it is on.
pub fn with_first_camera<R>(
    lab: &Lab,
    run: impl FnOnce(&mut dyn Capture, &Selected) -> R,
) -> Result<R, String> {
    let native: Box<dyn NativeLister> = match lab.lemnos_socket() {
        Some(socket) => Box::new(Daemon {
            socket: socket.to_path_buf(),
        }),
        None => Box::new(Nothing),
    };
    let (libcamera, v4l2): (Box<dyn LibcameraLister>, Box<dyn V4l2Lister>) = if lab.is_sim() {
        (Box::new(NoOther), Box::new(NoOther))
    } else {
        (Box::new(Libcamera), Box::new(Probe))
    };
    let plan = select(
        native.as_ref(),
        libcamera.as_ref(),
        v4l2.as_ref(),
        &SelectConfig::default(),
    )
    .map_err(|e| e.to_string())?;
    let selected = plan.cameras.first().ok_or("no camera is listed")?;
    match selected.owner {
        Backend::Native => {
            let socket = lab
                .lemnos_socket()
                .ok_or("a native camera needs the Lemnos daemon's socket (--lemnos SOCKET)")?;
            let client = Client::connect(socket, NAME).map_err(|e| format!("lemnos: {e}"))?;
            let found = discover(&client).map_err(|e| format!("lemnos discovery: {e}"))?;
            let camera = found
                .cameras
                .iter()
                .find(|c| c.name() == selected.key)
                .ok_or_else(|| format!("native camera {} is not on the daemon", selected.key))?;
            // The rate is the sensor mode's floor and the starting rate; the frame size is the
            // mode's own.
            let request = Request {
                width: 0,
                height: 0,
                fps_milli: FPS_MILLI,
                format: None,
                bit_depth: None,
                bayer: None,
            };
            let initial = Initial {
                exposure_us: EXPOSURE_US,
                gain_milli: GAIN_MILLI,
                frame_rate_milli: FPS_MILLI,
            };
            let clock = SystemClock::default();
            let session = Session::new(&client, camera, request, Policy::default(), &clock)
                .with_controls(initial);
            let mut capture = NativeCapture::start(session).map_err(|e| e.to_string())?;
            Ok(run(&mut capture, selected))
        }
        Backend::Libcamera => {
            let config = CaptureConfig {
                code: FALLBACK_CODE,
                width: FALLBACK_SIZE.0,
                height: FALLBACK_SIZE.1,
                buffers: 4,
            };
            let mut capture =
                LibcameraCapture::open(&selected.key, config).map_err(|e| e.to_string())?;
            Ok(run(&mut capture, selected))
        }
        Backend::V4l2 => {
            let mut config = StreamConfig::new(FALLBACK_CODE, FALLBACK_SIZE.0, FALLBACK_SIZE.1);
            config.interval = Interval::new(1_000, FPS_MILLI);
            let mut capture =
                V4l2Capture::start(&selected.key, config).map_err(|e| e.to_string())?;
            Ok(run(&mut capture, selected))
        }
    }
}

/// The value after `--name` on the command line, if it is given.
pub fn value(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next();
        }
    }
    None
}

/// Whether `--name` is on the command line.
pub fn flag(name: &str) -> bool {
    std::env::args().skip(1).any(|arg| arg == name)
}

/// Starts the camera service on `camera` (the id the chooser names), with its sockets in
/// `socket_dir`. The service runs the camera the way the service binary does: a worker, the
/// control socket, and one frame socket per camera.
///
/// `format` is the pixel format for a V4L2 or libcamera camera (`YUYV`, say). The simulated camera
/// is native, and takes its format from its sensor's mode, so the simulated run sets none.
pub fn start_service(
    lab: &Lab,
    socket_dir: &Path,
    camera: &str,
    format: Option<&str>,
) -> Result<Service, String> {
    let mut text = format!(
        "socket_dir = {:?}\nscan_ms = 100\ncameras = [{:?}]\n",
        socket_dir.display().to_string(),
        camera
    );
    if let Some(socket) = lab.lemnos_socket() {
        text.push_str(&format!(
            "lemnos_socket = {:?}\n",
            socket.display().to_string()
        ));
    }
    text.push_str(&format!(
        "[camera.{camera:?}]\nfps_milli = {FPS_MILLI}\nexposure_us = {EXPOSURE_US}\ngain_milli = {GAIN_MILLI}\n"
    ));
    if !lab.is_sim() {
        // Real cameras take the size and format asked here; the simulated camera takes its mode's.
        text.push_str(&format!(
            "width = {}\nheight = {}\n",
            FALLBACK_SIZE.0, FALLBACK_SIZE.1
        ));
        if let Some(format) = format {
            text.push_str(&format!("format = {format:?}\n"));
        }
    }
    let config = ServiceConfig::parse(&text).map_err(|e| e.to_string())?;
    let native: Arc<dyn NativeLister + Send + Sync> = match lab.lemnos_socket() {
        Some(socket) => Arc::new(Daemon {
            socket: socket.to_path_buf(),
        }),
        None => Arc::new(Nothing),
    };
    let (libcamera, v4l2): (
        Arc<dyn LibcameraLister + Send + Sync>,
        Arc<dyn V4l2Lister + Send + Sync>,
    ) = if lab.is_sim() {
        (Arc::new(NoOther), Arc::new(NoOther))
    } else {
        (Arc::new(Libcamera), Arc::new(Probe))
    };
    let deps = Deps {
        clock: Arc::new(SystemClock::default()),
        listers: Listers {
            native,
            libcamera,
            v4l2,
        },
        outlets: Arc::new(Discard),
    };
    Service::start(config, deps).map_err(|e| e.to_string())
}

impl std::fmt::Debug for Served {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Served")
            .field("dir", &self.dir)
            .field("camera", &self.camera)
            .finish()
    }
}

/// A camera service the example runs for itself, with its sockets in a directory it removes when
/// it is dropped. The service stops first (it is declared first).
pub struct Served {
    service: Service,
    _dir: DirGuard,
    dir: PathBuf,
    camera: String,
}

impl Served {
    /// The camera's frame socket: the service's per-camera socket for this camera.
    pub fn socket(&self) -> PathBuf {
        camera_socket(&self.dir, &self.camera)
    }

    /// The service, for its control socket and statuses.
    pub fn service(&self) -> &Service {
        &self.service
    }
}

/// Starts a camera service on `camera` in a directory of this process's own, named `name`. See
/// [`start_service`].
pub fn serve(lab: &Lab, name: &str, camera: &str, format: Option<&str>) -> Result<Served, String> {
    let dir = std::env::temp_dir().join(format!(
        "styx-examples-{name}-service-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let guard = DirGuard(dir.clone());
    let service = start_service(lab, &dir, camera, format)?;
    Ok(Served {
        service,
        _dir: guard,
        dir,
        camera: camera.to_string(),
    })
}

/// Connects to the camera socket at `path` and takes its offer. The service accepts a reader only
/// once it has a frame to give it, so a reader that comes too early is closed and tries again,
/// for up to ten seconds.
pub fn connect_reader(path: &Path) -> Result<Reader, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let attempt = UnixStream::connect(path)
            .map_err(|e| e.to_string())
            .and_then(|stream| Reader::connect(stream).map_err(|e| e.to_string()));
        match attempt {
            Ok(reader) => return Ok(reader),
            Err(e) if Instant::now() > deadline => {
                return Err(format!("{}: {e}", path.display()));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// The backends an example does not use: nothing listed.
#[derive(Debug)]
struct NoOther;

impl LibcameraLister for NoOther {
    fn list_libcamera(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

impl V4l2Lister for NoOther {
    fn list_v4l2(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}
