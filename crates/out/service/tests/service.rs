//! The service on the simulated backend: `lemnosd` in this process on `lemnos-sim`, with two
//! simulated cameras on one daemon, served through the control socket.

mod sim;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
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
use styx_service::{Config, Deps, Discard, Listers, Service};

/// Two complete chains (sensor, receiver, ISP each) on one daemon.
fn two_cameras() -> String {
    "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-cameras\"\n\
     [[devices]]\nid = \"isp0\"\ndriver = \"sim-isp\"\n\
     [[devices]]\nid = \"csi0\"\ndriver = \"sim-receiver\"\nlink = \"isp0\"\n\
     [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
     [[devices]]\nid = \"cam0-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi0\"\n\
     [[devices]]\nid = \"isp1\"\ndriver = \"sim-isp\"\n\
     [[devices]]\nid = \"csi1\"\ndriver = \"sim-receiver\"\nlink = \"isp1\"\n\
     [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
     [[devices]]\nid = \"cam1-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi1\"\n"
        .to_string()
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-service-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("test dir");
    dir
}

/// Starts lemnosd at `socket` with the board text.
fn daemon(socket: &Path, text: &str) -> LemnosDaemon {
    let config = DaemonConfig {
        socket: socket.to_path_buf(),
        name: "styx-service-test".to_string(),
        board: parse(text).expect("board"),
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

/// A clock that records each backoff wait and returns after a short real pause, so a failing
/// camera does not spin.
#[derive(Default)]
struct Recorder {
    waits: Mutex<Vec<Duration>>,
}

impl Recorder {
    fn waits(&self) -> Vec<Duration> {
        self.waits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
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
struct NoV4l2;

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

fn deps(clock: Arc<Recorder>, socket: &Path) -> Deps {
    Deps {
        clock,
        listers: Listers {
            native: Arc::new(styx_service::Daemon {
                socket: socket.to_path_buf(),
            }),
            libcamera: Arc::new(NoV4l2),
            v4l2: Arc::new(NoV4l2),
        },
        outlets: Arc::new(Discard),
    }
}

fn start_service(text: &str, clock: Arc<Recorder>, socket: &Path) -> Service {
    let config = Config::parse(text).expect("config");
    Service::start(config, deps(clock, socket)).expect("start the service")
}

fn wait_until(what: &str, within: Duration, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Sends one request and reads its answer lines (through the `ok` or `err` line).
fn ask(control: &Path, line: &str) -> Vec<String> {
    let mut stream = UnixStream::connect(control).expect("connect to the control socket");
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

fn camera_lines(control: &Path) -> Vec<String> {
    ask(control, "list")
        .into_iter()
        .filter(|l| l.starts_with("camera "))
        .collect()
}

/// The status line of camera `id`, if the service lists it.
fn line_of(control: &Path, id: &str) -> Option<String> {
    camera_lines(control)
        .into_iter()
        .find(|l| l.split_ascii_whitespace().nth(1) == Some(id))
}

fn state_of(control: &Path, id: &str) -> String {
    line_of(control, id)
        .and_then(|l| field(&l, "state").map(str::to_string))
        .unwrap_or_default()
}

fn frames_of(control: &Path, id: &str) -> u64 {
    line_of(control, id)
        .and_then(|l| field(&l, "frames").and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

fn copies() -> usize {
    styx_frame::copy_counters().iter().map(|c| c.copies()).sum()
}

#[test]
fn two_cameras_are_served_and_a_control_request_changes_exposure_on_one() {
    let dir = test_dir("two");
    let lemnos_socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&lemnos_socket, &two_cameras());
    let recorder = Arc::new(Recorder::default());
    let text = format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\n\
         [camera.\"lemnos:cam0-sensor\"]\nfps_milli = 60000\nexposure_us = 10000\n\
         [camera.\"lemnos:cam1-sensor\"]\nfps_milli = 60000\nexposure_us = 10000\n",
        dir.display(),
        lemnos_socket.display()
    );
    let service = start_service(&text, recorder, &lemnos_socket);
    let control = service.control_socket().to_path_buf();

    let cams = ["lemnos:cam0-sensor", "lemnos:cam1-sensor"];
    wait_until("both cameras streaming", Duration::from_secs(20), || {
        cams.iter()
            .all(|c| state_of(&control, c) == "streaming" && frames_of(&control, c) > 30)
    });
    for cam in cams {
        let line = line_of(&control, cam).expect("listed");
        assert_eq!(field(&line, "skipped"), Some("0"), "{line}");
        assert_eq!(field(&line, "owner"), Some("native"), "{line}");
    }

    // A control request that lands a few frames on: exposure 5 ms on camera 0 only.
    let last: u64 = field(&line_of(&control, cams[0]).expect("listed"), "last_id")
        .and_then(|v| v.parse().ok())
        .expect("last_id");
    let frame = last + 30;
    assert_eq!(
        ask(&control, &format!("set {} exposure 5000 {frame}", cams[0])),
        vec!["ok"]
    );
    wait_until("exposure 5000 on camera 0", Duration::from_secs(20), || {
        ask(&control, &format!("frame {}", cams[0]))
            .iter()
            .any(|l| field(l, "exposure_us") == Some("5000"))
    });
    let f0 = ask(&control, &format!("frame {}", cams[0]));
    let f1 = ask(&control, &format!("frame {}", cams[1]));
    assert_eq!(field(&f1[0], "exposure_us"), Some("10000"), "{f1:?}");
    assert!(
        field(&f0[0], "id")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap()
            >= frame,
        "{f0:?}"
    );
    let values = ask(&control, &format!("controls {}", cams[0]));
    assert!(
        values
            .iter()
            .any(|l| l.starts_with("control exposure ") && l.ends_with("value=5000")),
        "{values:?}"
    );

    assert_eq!(copies(), 0, "frames were copied");
    service.shutdown();
}

#[test]
fn a_camera_that_leaves_and_returns_is_served_again_with_capped_backoff() {
    let dir = test_dir("leave");
    let lemnos_socket = dir.join("lemnos.sock");
    let board = two_cameras();
    let lemnos = daemon(&lemnos_socket, &board);
    let recorder = Arc::new(Recorder::default());
    let text = format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\n\
         [camera.\"lemnos:cam0-sensor\"]\nfps_milli = 60000\n\
         [camera.\"lemnos:cam1-sensor\"]\nfps_milli = 60000\n",
        dir.display(),
        lemnos_socket.display()
    );
    let service = start_service(&text, Arc::clone(&recorder), &lemnos_socket);
    let control = service.control_socket().to_path_buf();
    let cam = "lemnos:cam0-sensor";
    let other = "lemnos:cam1-sensor";
    wait_until("both streaming", Duration::from_secs(20), || {
        state_of(&control, cam) == "streaming" && state_of(&control, other) == "streaming"
    });

    // Camera 1 is stopped by hand, so only camera 0's waits are recorded below.
    assert_eq!(ask(&control, &format!("stop {other}")), vec!["ok"]);
    wait_until("camera 1 stopped", Duration::from_secs(5), || {
        state_of(&control, other) == "stopped"
    });

    // The camera leaves: lemnosd goes away.
    drop(lemnos);
    wait_until("camera 0 backing off", Duration::from_secs(20), || {
        recorder.waits().len() >= 7
    });
    let line = line_of(&control, cam).expect("listed");
    assert_eq!(field(&line, "state"), Some("backoff"), "{line}");
    assert!(
        field(&line, "last_error").is_some_and(|e| e != "-"),
        "{line}"
    );
    assert_eq!(state_of(&control, other), "stopped");

    // It comes back: the same socket, the same board.
    let _lemnos = daemon(&lemnos_socket, &board);
    wait_until("camera 0 served again", Duration::from_secs(30), || {
        state_of(&control, cam) == "streaming" && frames_of(&control, cam) > 0
    });
    let waits = recorder.waits();
    let want = [250u64, 500, 1000, 2000, 4000, 5000, 5000];
    let got: Vec<u64> = waits
        .iter()
        .take(want.len())
        .map(|d| d.as_millis() as u64)
        .collect();
    assert_eq!(got, want, "backoff waits: {waits:?}");
    assert!(
        waits.iter().all(|d| *d <= Duration::from_secs(5)),
        "{waits:?}"
    );

    // Camera 1 starts again by hand and serves.
    assert_eq!(ask(&control, &format!("start {other}")), vec!["ok"]);
    wait_until("camera 1 streaming again", Duration::from_secs(20), || {
        state_of(&control, other) == "streaming"
    });
    assert_eq!(copies(), 0, "frames were copied");
    service.shutdown();
}

#[test]
fn auto_off_is_a_configured_policy_and_a_manual_start_brings_the_camera_back() {
    let dir = test_dir("auto-off");
    let lemnos_socket = dir.join("lemnos.sock");
    let board = two_cameras();
    let lemnos = daemon(&lemnos_socket, &board);
    let recorder = Arc::new(Recorder::default());
    let cam = "lemnos:cam0-sensor";
    let text = format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\nauto_off_after = 3\n\
         cameras = [\"{cam}\"]\n\
         [camera.\"{cam}\"]\nfps_milli = 60000\n",
        dir.display(),
        lemnos_socket.display()
    );
    let service = start_service(&text, Arc::clone(&recorder), &lemnos_socket);
    let control = service.control_socket().to_path_buf();
    wait_until("streaming", Duration::from_secs(20), || {
        state_of(&control, cam) == "streaming" && frames_of(&control, cam) > 0
    });

    // The camera leaves for good: three failures in a row turn it off, with two waits between.
    drop(lemnos);
    wait_until("camera off", Duration::from_secs(20), || {
        state_of(&control, cam) == "off"
    });
    let line = line_of(&control, cam).expect("listed");
    assert_eq!(field(&line, "failures"), Some("3"), "{line}");
    assert_eq!(
        recorder.waits(),
        vec![Duration::from_millis(250), Duration::from_millis(500)]
    );

    // Off stays off, however long the daemon is back: only a manual start brings it back.
    let _lemnos = daemon(&lemnos_socket, &board);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(state_of(&control, cam), "off");
    assert_eq!(ask(&control, &format!("start {cam}")), vec!["ok"]);
    wait_until(
        "streaming after a manual start",
        Duration::from_secs(20),
        || state_of(&control, cam) == "streaming",
    );
    service.shutdown();
}

#[test]
fn requests_are_checked_and_never_panic() {
    let dir = test_dir("requests");
    let lemnos_socket = dir.join("lemnos.sock");
    let service = start_service(
        &format!("socket_dir = \"{}\"\nscan_ms = 100\n", dir.display()),
        Arc::new(Recorder::default()),
        &lemnos_socket,
    );
    let control = service.control_socket().to_path_buf();
    assert_eq!(ask(&control, "list").last().map(String::as_str), Some("ok"));
    assert!(ask(&control, "nonsense")[0].starts_with("err"));
    assert!(ask(&control, "")[0].starts_with("err"));
    assert!(ask(&control, "set no-such exposure 1 2")[0].starts_with("err"));
    assert!(ask(&control, "set no-such bogus 1 2")[0].starts_with("err"));
    assert!(ask(&control, "set no-such exposure x 2")[0].starts_with("err"));
    assert!(ask(&control, "set no-such exposure 1 x")[0].starts_with("err"));
    assert!(ask(&control, "frame no-such")[0].starts_with("err"));
    let long_id = "x".repeat(300);
    assert!(ask(&control, &format!("start {long_id}"))[0].starts_with("err"));
    assert!(ask(&control, "a b c d e f g h i j")[0].starts_with("err"));
    // A line over the limit is refused and the connection is closed.
    let mut stream = UnixStream::connect(&control).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let big = format!("list {}\n", "y".repeat(1_000));
    stream.write_all(big.as_bytes()).expect("send");
    let mut answer = String::new();
    BufReader::new(stream)
        .read_line(&mut answer)
        .expect("answer");
    assert!(answer.starts_with("err"), "{answer}");
    // The service still answers.
    assert_eq!(ask(&control, "list").last().map(String::as_str), Some("ok"));
    service.shutdown();
}
