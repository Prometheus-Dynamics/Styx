//! A simulated camera's daemon, for a native session.
//!
//! [`SimCamera`] runs the chain itself: it opens the stream, starts the devices and grants each
//! frame. A native session (styx-native-run) runs the chain on its own, as the service does, so a
//! [`SimRig`] is only the daemon, its simulated drivers and the camera's frame clock. The clock makes
//! frames while it runs (the default is free running, as fast as the receiver takes them), stops
//! while it is paused (a silent camera, with no fault), and stops while the sensor reports a fault
//! or is dead. Faults are injected as on [`SimCamera`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_controls::Control;

use crate::camera::Error;
use crate::drivers::{self, SimIsp, SimReceiver, SimSensor};
use crate::silicon::{self, Fault, SensorTiming, Silicon};

/// The daemon's name for its clients.
const NAME: &str = "styx-sim-rig";

static DIRS: AtomicUsize = AtomicUsize::new(0);

/// A simulated camera's daemon and drivers, for a session to run. Dropping it stops the daemon.
pub struct SimRig {
    socket: PathBuf,
    token: String,
    dir: PathBuf,
    silicon: Arc<Silicon>,
    /// Last: the daemon stops after nothing else is using it.
    _service: Service,
}

impl SimRig {
    /// Starts the daemon with the default sensor timing, and runs the frame clock free.
    pub fn start() -> Result<Self, Error> {
        let n = DIRS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("styx-sim-rig-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).map_err(|e| Error::Daemon(e.to_string()))?;
        let token = dir
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let silicon = Arc::new(Silicon::new(SensorTiming::default()));
        silicon::register(&token, Arc::clone(&silicon));
        let started = Self::start_in(&dir, &token);
        match started {
            Ok(service) => {
                silicon.run_free();
                Ok(Self {
                    socket: dir.join("sock"),
                    token,
                    dir,
                    silicon,
                    _service: service,
                })
            }
            Err(e) => {
                silicon::unregister(&token);
                let _ = fs::remove_dir_all(&dir);
                Err(e)
            }
        }
    }

    fn start_in(dir: &Path, token: &str) -> Result<Service, Error> {
        let socket = dir.join("sock");
        let board = parse(&drivers::board(token, Some("csi0")))
            .map_err(|e| Error::Daemon(format!("board: {e:?}")))?;
        let config = Config {
            socket,
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
        Service::start(config).map_err(|e| Error::Daemon(format!("{e:?}")))
    }

    /// The daemon's socket: a session connects to it.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Injects `fault` into the simulated sensor (`None` clears it), as [`crate::SimCamera::inject_fault`].
    pub fn inject_fault(&self, fault: Option<Fault>) {
        self.silicon.set_fault(fault);
    }

    /// Runs the frame clock free: frames are made as the receiver takes them.
    pub fn run(&self) {
        self.silicon.run_free();
    }

    /// Stops the frame clock without a fault: the camera is silent until [`SimRig::run`].
    pub fn pause(&self) {
        self.silicon.pause();
    }

    /// Frames the clock has made since the camera started.
    pub fn frames_made(&self) -> u64 {
        self.silicon.granted()
    }

    /// Writes posted to the sensor since the camera started, through any instance.
    pub fn writes(&self) -> u64 {
        self.silicon.writes()
    }

    /// The code last written for `control`, whether or not it has landed yet. `None` when nothing
    /// was written (automatic) or the control is not one the simulated sensor takes.
    pub fn written(&self, control: Control) -> Option<u32> {
        silicon::lane_of(control).and_then(|lane| self.silicon.last_written(lane))
    }

    /// The sensor's code for an exposure in microseconds, as the simulated sensor writes it.
    pub fn exposure_code(us: u32) -> u32 {
        silicon::exposure_code(us)
    }

    /// The sensor's code for an analogue gain in thousandths, as the simulated sensor writes it.
    pub fn gain_code(milli: u32) -> u32 {
        silicon::gain_code(milli)
    }
}

impl Drop for SimRig {
    fn drop(&mut self) {
        silicon::unregister(&self.token);
        let _ = fs::remove_dir_all(&self.dir);
    }
}
