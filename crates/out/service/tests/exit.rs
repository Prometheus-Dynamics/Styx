//! The service process on the simulated board. The service is the real binary, a child process;
//! the simulated board is `lemnosd` in this process, so the simulator's call log is this
//! process's. The tests check that a signal stops every camera through the Lemnos contract before
//! the process leaves, that the next start streams at once, and that a sensor a previous owner
//! left started is stopped at start.

use styx_sim::drivers::{SimIsp, SimReceiver, SimSensor};

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use lemnos_contract_camera_sensor::CameraSensor;
use lemnos_core::Status;
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config as DaemonConfig, Service as LemnosDaemon};
use styx_lemnos::Client;
use styx_service::{Config, camera_socket};

const CAMERAS: [&str; 2] = ["lemnos:cam0-sensor", "lemnos:cam1-sensor"];

/// The simulator's call log is process-wide, so the tests in this file run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// Two complete chains (sensor, receiver, ISP each) on one daemon.
fn two_cameras() -> String {
    "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-cameras\"\n\
     [[devices]]\nid = \"isp0\"\ndriver = \"sim-isp\"\n\
     [[devices]]\nid = \"csi0\"\ndriver = \"sim-receiver\"\nlink = \"isp0\"\nconfig = { layout = false }\n\
     [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
     [[devices]]\nid = \"cam0-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi0\"\nconfig = { modes = \"run\", drop_frames = 2 }\n\
     [[devices]]\nid = \"isp1\"\ndriver = \"sim-isp\"\n\
     [[devices]]\nid = \"csi1\"\ndriver = \"sim-receiver\"\nlink = \"isp1\"\nconfig = { layout = false }\n\
     [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
     [[devices]]\nid = \"cam1-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi1\"\nconfig = { modes = \"run\", drop_frames = 2 }\n"
        .to_string()
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-exit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("test dir");
    dir
}

/// Starts lemnosd at `socket` with the board text.
fn daemon(socket: &Path) -> LemnosDaemon {
    let config = DaemonConfig {
        socket: socket.to_path_buf(),
        name: "styx-service-exit-test".to_string(),
        board: parse(&two_cameras()).expect("board"),
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
    let daemon = LemnosDaemon::start(config).expect("start lemnosd");
    // The daemon answers once its socket is up.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Client::connect(socket, "styx-service-exit-test").is_err() {
        assert!(Instant::now() < deadline, "lemnosd is not answering");
        std::thread::sleep(Duration::from_millis(10));
    }
    daemon
}

fn config_text(dir: &Path, lemnos_socket: &Path) -> String {
    format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\n\
         cameras = [\"lemnos:cam0-sensor\", \"lemnos:cam1-sensor\"]\n\
         [camera.\"lemnos:cam0-sensor\"]\nfps_milli = 60000\nexposure_us = 10000\n\
         [camera.\"lemnos:cam1-sensor\"]\nfps_milli = 60000\nexposure_us = 10000\n",
        dir.display(),
        lemnos_socket.display()
    )
}

/// A running service process. It is killed when dropped, whatever the test did.
struct Process {
    child: Option<Child>,
    control: PathBuf,
}

impl Process {
    /// Starts the service binary on the config, its stderr in `service-<run>.log`.
    fn spawn(dir: &Path, text: &str, run: u32) -> Self {
        let config = dir.join(format!("service-{run}.toml"));
        std::fs::write(&config, text).expect("write the config");
        let log = File::create(dir.join(format!("service-{run}.log"))).expect("log file");
        let child = Command::new(env!("CARGO_BIN_EXE_styx-service"))
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("start the service binary");
        let control = Config::parse(text).expect("config").control_socket();
        // The control socket is bound once the service has started.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !control.exists() {
            assert!(
                Instant::now() < deadline,
                "no control socket {}",
                control.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Self {
            child: Some(child),
            control,
        }
    }

    /// Sends a signal (`-TERM` or `-INT`) with the system's `kill`.
    fn signal(&self, how: &str) {
        let pid = self.child.as_ref().expect("running").id();
        let sent = Command::new("kill")
            .args([how, &pid.to_string()])
            .status()
            .expect("run kill");
        assert!(sent.success(), "kill {how} {pid} failed");
    }

    /// Waits for the process to exit, and returns how it did.
    fn exit(&mut self, within: Duration) -> ExitStatus {
        let mut child = self.child.take().expect("running");
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = child.try_wait().expect("wait") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the service did not exit within {within:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Sends one request and reads its answer lines (through the `ok` or `err` line).
fn ask(control: &Path, line: &str) -> Vec<String> {
    use std::io::{BufRead, BufReader, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(control).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    writeln!(stream, "{line}").expect("send");
    let mut reader = BufReader::new(stream);
    let mut out = Vec::new();
    loop {
        let mut answer = String::new();
        let n = reader.read_line(&mut answer).expect("read the answer");
        assert!(n > 0, "the control socket closed before answering {line:?}");
        let answer = answer.trim_end().to_string();
        let last = answer == "ok" || answer.starts_with("err");
        out.push(answer);
        if last {
            return out;
        }
    }
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_ascii_whitespace()
        .find_map(|w| w.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

/// The status line of camera `id`, if the service lists it.
fn line_of(control: &Path, id: &str) -> Option<String> {
    ask(control, "list")
        .into_iter()
        .filter(|l| l.starts_with("camera "))
        .find(|l| l.split_ascii_whitespace().nth(1) == Some(id))
}

fn wait_until(what: &str, within: Duration, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until every camera streams and has delivered frames.
fn wait_streaming(control: &Path, cameras: &[&str]) {
    wait_until("the cameras streaming", Duration::from_secs(30), || {
        cameras.iter().all(|c| {
            line_of(control, c).is_some_and(|l| {
                field(&l, "state") == Some("streaming")
                    && field(&l, "frames")
                        .and_then(|v| v.parse::<u64>().ok())
                        .is_some_and(|f| f > 30)
            })
        })
    });
}

/// Asks the sensor on the daemon to select a mode, through the contract. `Ok` when it is not
/// streaming; `BUSY` while it streams.
fn select_mode(lemnos_socket: &Path, sensor: &str) -> Result<(), Status> {
    let client = Client::connect(lemnos_socket, "styx-service-exit-test").expect("connect");
    let mut device = client
        .open::<dyn CameraSensor>(sensor)
        .expect("open the sensor");
    device.select_mode(0)
}

fn count(log: &[String], entry: &str) -> usize {
    log.iter().filter(|l| l.as_str() == entry).count()
}

fn sockets_left(dir: &Path) -> Vec<PathBuf> {
    CAMERAS
        .iter()
        .map(|c| camera_socket(dir, c))
        .filter(|p| p.exists())
        .collect()
}

/// A signal stops every camera through the contract before the process leaves, and the next start
/// streams at once: no busy refusal and no backoff.
fn stop_then_restart(signal: &str) {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = test_dir(&format!("stop{}", signal.trim_start_matches('-')));
    let lemnos_socket = dir.join("lemnos.sock");
    let _daemon = daemon(&lemnos_socket);
    let text = config_text(&dir, &lemnos_socket);

    let mut first = Process::spawn(&dir, &text, 1);
    wait_streaming(&first.control, &CAMERAS);
    let _ = styx_sim::take_log();

    first.signal(signal);
    let status = first.exit(Duration::from_secs(30));
    assert!(status.success(), "the service exited with {status:?}");

    // Every camera's sensor and receiver were stopped through the contract, in this process's
    // simulator: one stop of each per camera.
    let log = styx_sim::take_log();
    assert!(
        count(&log, "sensor.stop") >= CAMERAS.len(),
        "sensor stops in {log:?}"
    );
    assert!(
        count(&log, "receiver.stop") >= CAMERAS.len(),
        "receiver stops in {log:?}"
    );
    // Asked through the contract too: a sensor that is not streaming takes a mode.
    for sensor in ["cam0-sensor", "cam1-sensor"] {
        assert_eq!(
            select_mode(&lemnos_socket, sensor),
            Ok(()),
            "{sensor} is still streaming after {signal}"
        );
    }
    assert!(!first.control.exists(), "the control socket is left");
    assert_eq!(
        sockets_left(&dir),
        Vec::<PathBuf>::new(),
        "camera sockets left"
    );

    // The next start streams at once.
    let mut second = Process::spawn(&dir, &text, 2);
    wait_streaming(&second.control, &CAMERAS);
    for camera in CAMERAS {
        let line = line_of(&second.control, camera).expect("listed");
        assert_eq!(field(&line, "total_failures"), Some("0"), "{line}");
        assert_eq!(field(&line, "recoveries"), Some("0"), "{line}");
        assert_eq!(field(&line, "next_delay_ms"), Some("-"), "{line}");
    }
    second.signal(signal);
    let status = second.exit(Duration::from_secs(30));
    assert!(
        status.success(),
        "the second service exited with {status:?}"
    );
}

#[test]
fn sigterm_stops_every_camera_and_the_next_start_streams_at_once() {
    stop_then_restart("-TERM");
}

#[test]
fn sigint_stops_every_camera_and_the_next_start_streams_at_once() {
    stop_then_restart("-INT");
}

/// A sensor left streaming by a previous owner refuses a mode. The service stops the sensor and
/// receiver once through the contract, says so in its status, and streams.
#[test]
fn a_sensor_left_started_by_a_previous_owner_is_stopped_at_start_and_the_service_streams() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = test_dir("left");
    let lemnos_socket = dir.join("lemnos.sock");
    let _daemon = daemon(&lemnos_socket);
    let text = config_text(&dir, &lemnos_socket);

    // The previous owner: its sensor started by hand through the contract, no service.
    {
        let client = Client::connect(&lemnos_socket, "previous-owner").expect("connect");
        let mut sensor = client
            .open::<dyn CameraSensor>("cam0-sensor")
            .expect("open the sensor");
        sensor.select_mode(0).expect("select a mode by hand");
        sensor.start().expect("start the sensor by hand");
    }
    assert_eq!(
        select_mode(&lemnos_socket, "cam0-sensor"),
        Err(Status::BUSY),
        "the sensor left by hand should be streaming"
    );
    let _ = styx_sim::take_log();

    let mut service = Process::spawn(&dir, &text, 1);
    wait_streaming(&service.control, &CAMERAS);

    let cam0 = line_of(&service.control, CAMERAS[0]).expect("listed");
    assert_eq!(field(&cam0, "recoveries"), Some("1"), "{cam0}");
    assert!(
        field(&cam0, "last_recovery").is_some_and(|v| v.contains("stopped")),
        "{cam0}"
    );
    let cam1 = line_of(&service.control, CAMERAS[1]).expect("listed");
    assert_eq!(field(&cam1, "recoveries"), Some("0"), "{cam1}");

    let log = styx_sim::take_log();
    assert!(
        count(&log, "sensor.stop") >= 1,
        "the left sensor was not stopped: {log:?}"
    );

    service.signal("-TERM");
    let status = service.exit(Duration::from_secs(30));
    assert!(status.success(), "the service exited with {status:?}");
}
