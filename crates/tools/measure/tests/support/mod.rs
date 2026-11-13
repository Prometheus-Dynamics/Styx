//! Shared test code: the simulated board, lemnosd in this process, and the service in this
//! process on the simulated camera. Test code only. Not every test binary uses every helper.

#![allow(dead_code)]

#[path = "sim.rs"]
pub mod sim;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config as DaemonConfig, Service as LemnosDaemon};
use styx_native_run::Clock;
use styx_select::listing::{LibcameraLister, ListError, Listed, V4l2Lister};
use styx_service::{Config, Deps, Discard, Listers, Service, camera_socket};

pub const CAMERA: &str = "lemnos:cam0-sensor";

/// A clock that records each backoff wait and returns after a short real pause.
#[derive(Default)]
pub struct Recorder {
    waits: Mutex<Vec<Duration>>,
}

impl Clock for Recorder {
    fn now(&self) -> Duration {
        Duration::ZERO
    }

    fn wait(&self, for_: Duration, _cancel: &AtomicBool) {
        self.waits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(for_);
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[derive(Debug)]
pub struct NoV4l2;

impl V4l2Lister for NoV4l2 {
    fn list_v4l2(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

impl LibcameraLister for NoV4l2 {
    fn list_libcamera(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

pub fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-measure-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("test dir");
    dir
}

/// lemnosd in this process on the one-camera simulated board.
pub fn daemon(socket: &Path) -> LemnosDaemon {
    let config = DaemonConfig {
        socket: socket.to_path_buf(),
        name: "styx-measure-test".to_string(),
        board: parse(&sim::board(Some("csi0"))).expect("board"),
        drivers: vec![
            entry::<sim::SimSensor>(),
            entry::<sim::SimReceiver>(),
            entry::<sim::SimIsp>(),
        ],
        backend: Box::new(SimBoard::new()),
        options: Options {
            call_deadline: Duration::from_millis(200),
            ..Options::default()
        },
    };
    LemnosDaemon::start(config).expect("start lemnosd")
}

/// The service on the simulated camera at 120 fps (mode 0, 320 x 40), served from `dir`.
pub fn start_service(dir: &Path, lemnos_socket: &Path) -> Service {
    let text = format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\n\
         [camera.\"{CAMERA}\"]\nwidth = 320\nheight = 40\nfps_milli = 120000\nexposure_us = 4000\n",
        dir.display(),
        lemnos_socket.display()
    );
    let config = Config::parse(&text).expect("config");
    let deps = Deps {
        clock: Arc::new(Recorder::default()),
        listers: Listers {
            native: Arc::new(styx_service::Daemon {
                socket: lemnos_socket.to_path_buf(),
            }),
            libcamera: Arc::new(NoV4l2),
            v4l2: Arc::new(NoV4l2),
        },
        outlets: Arc::new(Discard),
    };
    Service::start(config, deps).expect("start the service")
}

pub fn socket_of(dir: &Path) -> PathBuf {
    camera_socket(dir, CAMERA)
}

pub fn wait_until(what: &str, within: Duration, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The control socket's `list` answer.
pub fn list(service: &Service) -> Vec<String> {
    styx_measure::control::ask(service.control_socket(), "list").expect("list")
}

/// Waits until the camera streams and publishes its frames on its socket.
pub fn wait_publishing(service: &Service) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let lines = list(service);
        if lines.iter().any(|l| {
            l.contains(&format!("camera {CAMERA} "))
                && l.contains("state=streaming")
                && l.contains("publish=publishing")
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the camera did not publish; the list answer:\n{}",
            lines.join("\n")
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
