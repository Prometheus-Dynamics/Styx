//! The service on the simulated backend: `lemnosd` in this process on `lemnos-sim`, with two
//! simulated cameras on one daemon, served through the control socket.

mod sim;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use lemnos_contract_camera_sensor::frame_length_for_fps_milli;
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config as DaemonConfig, Service as LemnosDaemon};
use styx_controls::FrameMetadata;
use styx_frame::{FrameLease, copy_counters};
use styx_native_run::Clock;
use styx_select::listing::{LibcameraLister, ListError, Listed, V4l2Lister};
use styx_service::{Config, Deps, Discard, Listers, Outlet, Outlets, Service, camera_socket};
use styx_socket::Reader;

/// Two complete chains (sensor, receiver, ISP each) on one daemon.
/// Mode 0's shortest frame in the double (asked at 120 fps, the contract's whole line nearest that
/// rate), and the exposure ceiling that frame gives: (frame - the mode's margin of 25 lines) at its
/// 9.1 us line, rounded to the microsecond.
fn mode0_shortest() -> (u32, u32) {
    let (lines, _) = frame_length_for_fps_milli(9_100_000, 1, u32::MAX, 120_000).expect("a frame");
    let us = (u64::from(lines) - 25) * 9_100_000;
    (
        lines,
        u32::try_from((us + 500_000) / 1_000_000).expect("us"),
    )
}

/// Two cameras, each with its own receiver and ISP. Each call names its cameras' frame clocks
/// uniquely, so the tests of this binary (which run in one process) do not share one.
fn two_cameras() -> String {
    static BOARDS: AtomicU32 = AtomicU32::new(0);
    let n = BOARDS.fetch_add(1, Ordering::Relaxed);
    format!(
        "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-cameras\"\n\
         [[devices]]\nid = \"isp0\"\ndriver = \"sim-isp\"\n\
         [[devices]]\nid = \"csi0\"\ndriver = \"sim-receiver\"\nlink = \"isp0\"\n\
         config = {{ camera = \"cam0-{n}\" }}\n\
         [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
         [[devices]]\nid = \"cam0-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi0\"\n\
         config = {{ camera = \"cam0-{n}\" }}\n\
         [[devices]]\nid = \"isp1\"\ndriver = \"sim-isp\"\n\
         [[devices]]\nid = \"csi1\"\ndriver = \"sim-receiver\"\nlink = \"isp1\"\n\
         config = {{ camera = \"cam1-{n}\" }}\n\
         [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
         [[devices]]\nid = \"cam1-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi1\"\n\
         config = {{ camera = \"cam1-{n}\" }}\n"
    )
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
        // Whether the service's camera thread kept up with the simulated camera is the
        // machine's doing (a loaded machine holds it up and the camera drops frames), so zero
        // skips is not this test's claim. What must hold always: the counts add up.
        let number = |key: &str| -> u64 {
            field(&line, key)
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("{key}: {line}"))
        };
        let (frames, skipped, last_id) = (number("frames"), number("skipped"), number("last_id"));
        assert!(
            frames + skipped <= last_id + 1,
            "{frames} frames and {skipped} skipped do not fit ids up to {last_id}: {line}"
        );
        if skipped != 0 {
            println!("{cam}: {skipped} frames skipped in {frames} (the camera thread was held up)");
        }
        assert_eq!(field(&line, "owner"), Some("native"), "{line}");
        assert_eq!(field(&line, "health"), Some("healthy"), "{line}");
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
    // The change shows from the asked frame on: the newest frame read has it, so it is on or past
    // that frame.
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
fn a_rate_above_the_mode_ceiling_is_answered_with_the_ceiling_and_the_status_says_why() {
    let dir = test_dir("ceiling");
    let lemnos_socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&lemnos_socket, &two_cameras());
    let recorder = Arc::new(Recorder::default());
    // 240 fps is above both simulated modes (120 and 60 fps): the fastest candidate runs at its
    // ceiling, and the status says what was asked, what it gives and why.
    let text = format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\n\
         [camera.\"lemnos:cam0-sensor\"]\nwidth = 320\nheight = 40\nfps_milli = 240000\n\
         exposure_us = 5000\n\
         [camera.\"lemnos:cam1-sensor\"]\nfps_milli = 60000\n",
        dir.display(),
        lemnos_socket.display()
    );
    let service = start_service(&text, recorder, &lemnos_socket);
    let control = service.control_socket().to_path_buf();
    let cam = "lemnos:cam0-sensor";
    wait_until("camera 0 streaming", Duration::from_secs(20), || {
        state_of(&control, cam) == "streaming" && frames_of(&control, cam) > 30
    });
    let line = line_of(&control, cam).expect("listed");
    assert_eq!(field(&line, "requested"), Some("240000"), "{line}");
    assert_eq!(field(&line, "mode"), Some("0"), "{line}");
    assert_eq!(field(&line, "format"), Some("pRAA"), "{line}");
    assert_eq!(field(&line, "bits"), Some("10"), "{line}");
    assert_eq!(
        field(&line, "mode_reason"),
        Some("rate_clamped_to_ceiling"),
        "{line}"
    );
    let why = field(&line, "mode_text").expect("the mode's reason");
    assert!(why.contains("asked_240.000_fps"), "{line}");
    let achieved: u32 = field(&line, "achieved")
        .and_then(|v| v.parse().ok())
        .expect("the achieved rate");
    assert!(
        (100_000..240_000).contains(&achieved),
        "achieved {achieved}: {line}"
    );
    service.shutdown();
}

#[test]
fn an_exposure_the_frame_cannot_hold_streams_clamped_with_the_reason_and_no_backoff() {
    let dir = test_dir("exposure-ceiling");
    let lemnos_socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&lemnos_socket, &two_cameras());
    let recorder = Arc::new(Recorder::default());
    // 240 fps is past mode 0's ceiling, so the frame is its shortest (`mode0_shortest`). Its exposure
    // ceiling is (the frame - 25) lines of 9.1 us. The 10 ms asked is moved to it, and the
    // camera streams: a clamp is not a failure.
    let text = format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\n\
         [camera.\"lemnos:cam0-sensor\"]\nwidth = 320\nheight = 40\nfps_milli = 240000\n\
         exposure_us = 10000\n\
         [camera.\"lemnos:cam1-sensor\"]\nfps_milli = 60000\n",
        dir.display(),
        lemnos_socket.display()
    );
    let service = start_service(&text, recorder, &lemnos_socket);
    let control = service.control_socket().to_path_buf();
    let cam = "lemnos:cam0-sensor";
    wait_until("camera 0 streaming", Duration::from_secs(20), || {
        state_of(&control, cam) == "streaming" && frames_of(&control, cam) > 30
    });
    let line = line_of(&control, cam).expect("listed");
    assert_eq!(field(&line, "state"), Some("streaming"), "{line}");
    assert_eq!(field(&line, "failures"), Some("0"), "{line}");
    assert_eq!(field(&line, "total_failures"), Some("0"), "{line}");
    assert_eq!(field(&line, "starts"), Some("1"), "{line}");
    assert_eq!(field(&line, "exposure_requested"), Some("10000"), "{line}");
    let (lines, ceiling) = mode0_shortest();
    let ceiling = ceiling.to_string();
    assert_eq!(
        field(&line, "exposure_applied"),
        Some(ceiling.as_str()),
        "{line}"
    );
    let why = field(&line, "exposure_clamp").expect("the clamp's reason");
    assert!(
        why.contains(&format!("the_frame_is_{lines}_lines")),
        "{line}"
    );
    assert!(why.contains(&format!("at_most_{ceiling}_us")), "{line}");
    // The frames show what was applied.
    let frame = ask(&control, &format!("frame {cam}"));
    assert_eq!(
        field(&frame[0], "exposure_us"),
        Some(ceiling.as_str()),
        "{frame:?}"
    );
    service.shutdown();
}

#[test]
fn a_set_past_the_frame_is_answered_ok_and_applied_clamped_with_the_reason() {
    let dir = test_dir("exposure-set");
    let lemnos_socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&lemnos_socket, &two_cameras());
    let recorder = Arc::new(Recorder::default());
    let text = format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\n\
         [camera.\"lemnos:cam0-sensor\"]\nwidth = 320\nheight = 40\nfps_milli = 240000\n\
         exposure_us = 5000\n\
         [camera.\"lemnos:cam1-sensor\"]\nfps_milli = 60000\n",
        dir.display(),
        lemnos_socket.display()
    );
    let service = start_service(&text, recorder, &lemnos_socket);
    let control = service.control_socket().to_path_buf();
    let cam = "lemnos:cam0-sensor";
    wait_until("camera 0 streaming", Duration::from_secs(20), || {
        state_of(&control, cam) == "streaming" && frames_of(&control, cam) > 30
    });
    let line = line_of(&control, cam).expect("listed");
    assert_eq!(field(&line, "exposure_clamp"), Some("-"), "{line}");
    assert_eq!(field(&line, "exposure_applied"), Some("4996"), "{line}");

    // 20 ms, a few frames on: past the frame's ceiling. The answer is ok, not an error.
    let last: u64 = field(&line, "last_id")
        .and_then(|v| v.parse().ok())
        .expect("last_id");
    let frame = last + 30;
    assert_eq!(
        ask(&control, &format!("set {cam} exposure 20000 {frame}")),
        vec!["ok"]
    );
    wait_until("the clamp in status", Duration::from_secs(20), || {
        line_of(&control, cam).is_some_and(|l| field(&l, "exposure_requested") == Some("20000"))
    });
    let (_, ceiling) = mode0_shortest();
    let ceiling = ceiling.to_string();
    wait_until("the ceiling on frames", Duration::from_secs(20), || {
        ask(&control, &format!("frame {cam}"))
            .iter()
            .any(|l| field(l, "exposure_us") == Some(ceiling.as_str()))
    });
    let line = line_of(&control, cam).expect("listed");
    let (_, ceiling) = mode0_shortest();
    let ceiling = ceiling.to_string();
    assert_eq!(
        field(&line, "exposure_applied"),
        Some(ceiling.as_str()),
        "{line}"
    );
    assert!(
        field(&line, "exposure_clamp")
            .is_some_and(|c| c.contains(&format!("at_most_{ceiling}_us"))),
        "{line}"
    );
    assert_eq!(field(&line, "state"), Some("streaming"), "{line}");
    assert_eq!(field(&line, "total_failures"), Some("0"), "{line}");
    service.shutdown();
}

#[test]
fn an_unknown_format_is_a_status_error_that_names_the_formats_the_modes_have() {
    let dir = test_dir("format");
    let lemnos_socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&lemnos_socket, &two_cameras());
    let recorder = Arc::new(Recorder::default());
    // The simulated modes are both 10-bit (pRAA): an 8-bit format is not among them.
    let text = format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\n\
         [camera.\"lemnos:cam0-sensor\"]\nformat = \"RGGB\"\nfps_milli = 60000\n\
         [camera.\"lemnos:cam1-sensor\"]\nfps_milli = 60000\n",
        dir.display(),
        lemnos_socket.display()
    );
    let service = start_service(&text, recorder, &lemnos_socket);
    let control = service.control_socket().to_path_buf();
    let cam = "lemnos:cam0-sensor";
    // last_error is free text at the end of the line: everything after `last_error=`.
    let error_of = |line: &str| line.split_once("last_error=").map(|(_, e)| e.to_string());
    wait_until("camera 0's error", Duration::from_secs(20), || {
        line_of(&control, cam)
            .is_some_and(|l| error_of(&l).is_some_and(|e| e.contains("has format RGGB")))
    });
    let line = line_of(&control, cam).expect("listed");
    assert_ne!(state_of(&control, cam), "streaming", "{line}");
    assert!(
        error_of(&line).is_some_and(|e| e.contains("the modes have pRAA")),
        "{line}"
    );
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

// ---------------------------------------------------------------------------------------------
// Readers in other processes. The test binary runs itself as a reader (`reader_child`), one per
// camera, and each child reports on stdout what it received.

const CHILD_SOCKET: &str = "STYX_SERVICE_TEST_READER_SOCKET";
const CHILD_STOP: &str = "STYX_SERVICE_TEST_READER_STOP";

fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Whether the producer has closed `socket` (a reader's end of its connection).
fn closed(socket: &UnixStream) -> bool {
    let Ok(mut other) = socket.try_clone() else {
        return true;
    };
    if other.set_nonblocking(true).is_err() {
        return true;
    }
    let mut byte = [0_u8; 1];
    let ended = matches!(other.read(&mut byte), Ok(0));
    let _ = other.set_nonblocking(false);
    ended
}

/// The child process: connects to the camera socket named in the environment, takes frames, and
/// reports each on stdout, until the file named in the environment exists (the parent makes it).
/// Connects again when the producer closes its connection. Does nothing when the environment does
/// not name a socket, so the normal test run passes it by.
#[test]
fn reader_child() {
    let Ok(socket) = std::env::var(CHILD_SOCKET) else {
        return;
    };
    let stop = PathBuf::from(std::env::var(CHILD_STOP).expect("the stop file's path"));
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut reader: Option<Reader> = None;
    let mut told = false;
    while !stop.exists() && Instant::now() < deadline {
        let Some(current) = reader.as_mut() else {
            match UnixStream::connect(&socket)
                .and_then(|s| Reader::connect(s).map_err(|e| std::io::Error::other(e.to_string())))
            {
                Ok(connected) => {
                    println!("STYX CONNECTED");
                    reader = Some(connected);
                }
                Err(e) => {
                    if !told {
                        eprintln!("STYX reader child: connect {socket}: {e}");
                        told = true;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            continue;
        };
        match current.next_frame() {
            Ok(Some(frame)) => {
                let id = frame.lease().id();
                let meta = FrameMetadata::from_bytes(frame.user());
                let checksum = {
                    let read = frame.lease().read().expect("mapped for the CPU");
                    fnv(read.plane(0).expect("plane").data)
                };
                match meta {
                    Some(m) => println!(
                        "STYX FRAME id={id} exposure={} checksum={checksum:016x}",
                        m.exposure_us
                    ),
                    None => println!("STYX FRAME id={id} exposure=bad checksum={checksum:016x}"),
                }
            }
            Ok(None) => {
                if closed(current.socket()) {
                    println!("STYX CLOSED");
                    reader = None;
                } else {
                    let _ = current.wait(Some(Duration::from_millis(5)));
                }
            }
            Err(_) => {
                println!("STYX CLOSED");
                reader = None;
            }
        }
    }
    let copies: usize = copy_counters().iter().map(|c| c.copies()).sum();
    println!("STYX COPIES {copies}");
}

/// A reader child process and what it prints.
struct ChildReader {
    lines: Receiver<String>,
    child: Child,
    stop: PathBuf,
    lines_seen: Vec<String>,
}

impl ChildReader {
    fn spawn(dir: &Path, socket: &Path, name: &str) -> Self {
        let stop = dir.join(format!("{name}.stop"));
        let _ = std::fs::remove_file(&stop);
        let exe = std::env::current_exe().expect("the test binary");
        let mut child = Command::new(exe)
            .args(["--exact", "reader_child", "--nocapture", "--test-threads=1"])
            .env(CHILD_SOCKET, socket)
            .env(CHILD_STOP, &stop)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn a reader process");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            lines,
            child,
            stop,
            lines_seen: Vec::new(),
        }
    }

    /// Reads what the child has printed for up to `within`, keeping it. Its markers start with
    /// `STYX `; anything before that on the line (libtest's `test reader_child ... `) is dropped.
    fn pump(&mut self, within: Duration) {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left.max(Duration::from_millis(1))) {
                Ok(line) => {
                    if let Some(at) = line.find("STYX ") {
                        self.lines_seen.push(line[at + 5..].to_string());
                    }
                }
                Err(_) => break,
            }
            if Instant::now() >= deadline {
                break;
            }
        }
    }

    fn has(&self, want: impl Fn(&str) -> bool) -> bool {
        self.lines_seen.iter().any(|l| want(l))
    }

    fn frames(&self) -> Vec<(u64, u32, u64)> {
        frame_lines(&self.lines_seen)
    }

    /// Tells the child to stop, and reads until it has reported its copies and exited.
    fn finish(&mut self) {
        let _ = std::fs::write(&self.stop, b"stop");
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline && !self.has(|l| l.starts_with("COPIES")) {
            self.pump(Duration::from_millis(100));
        }
    }
}

impl Drop for ChildReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn frame_lines(lines: &[String]) -> Vec<(u64, u32, u64)> {
    lines
        .iter()
        .filter_map(|l| l.strip_prefix("FRAME "))
        .filter_map(|rest| {
            let id = field(rest, "id")?.parse().ok()?;
            let exposure = field(rest, "exposure")?.parse().ok()?;
            let checksum = u64::from_str_radix(field(rest, "checksum")?, 16).ok()?;
            Some((id, exposure, checksum))
        })
        .collect()
}

/// The checksum of each frame each camera's outlet saw, by camera and frame id.
type Seen = Arc<Mutex<BTreeMap<(String, u64), u64>>>;

struct RecordingOutlet {
    camera: String,
    seen: Seen,
}

impl Outlet for RecordingOutlet {
    fn frame(&mut self, _camera: &str, lease: &FrameLease<FrameMetadata>) {
        let read = lease.read().expect("mapped for the CPU");
        let checksum = fnv(read.plane(0).expect("plane").data);
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((self.camera.clone(), lease.id()), checksum);
    }
}

struct RecordingOutlets(Seen);

impl Outlets for RecordingOutlets {
    fn attach(&self, camera: &str) -> Box<dyn Outlet> {
        Box::new(RecordingOutlet {
            camera: camera.to_string(),
            seen: Arc::clone(&self.0),
        })
    }
}

fn start_recording_service(lemnos_socket: &Path, text: &str, seen: Seen) -> Service {
    let config = Config::parse(text).expect("config");
    let mut deps = deps(Arc::new(Recorder::default()), lemnos_socket);
    deps.outlets = Arc::new(RecordingOutlets(seen));
    Service::start(config, deps).expect("start the service")
}

#[test]
fn frames_reach_readers_in_other_processes_with_their_metadata_and_bytes() {
    let dir = test_dir("readers");
    let lemnos_socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&lemnos_socket, &two_cameras());
    let seen: Seen = Arc::new(Mutex::new(BTreeMap::new()));
    let text = format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\n\
         [camera.\"lemnos:cam0-sensor\"]\nfps_milli = 60000\nexposure_us = 10000\n\
         [camera.\"lemnos:cam1-sensor\"]\nfps_milli = 60000\nexposure_us = 10000\n",
        dir.display(),
        lemnos_socket.display()
    );
    let service = start_recording_service(&lemnos_socket, &text, Arc::clone(&seen));
    let control = service.control_socket().to_path_buf();
    let cams = ["lemnos:cam0-sensor", "lemnos:cam1-sensor"];
    wait_until("both cameras streaming", Duration::from_secs(20), || {
        cams.iter().all(|c| state_of(&control, c) == "streaming")
    });

    let mut readers: Vec<ChildReader> = cams
        .iter()
        .enumerate()
        .map(|(i, c)| ChildReader::spawn(&dir, &camera_socket(&dir, c), &format!("reader{i}")))
        .collect();
    wait_until("both readers connected", Duration::from_secs(30), || {
        for reader in readers.iter_mut() {
            reader.pump(Duration::from_millis(20));
        }
        readers.iter().all(|r| r.has(|l| l == "CONNECTED"))
    });
    for cam in cams {
        let line = line_of(&control, cam).expect("listed");
        assert_eq!(field(&line, "publish"), Some("publishing"), "{line}");
    }

    // A control request from here on: exposure 5 ms on camera 0 only, a few frames on.
    let last: u64 = field(&line_of(&control, cams[0]).expect("listed"), "last_id")
        .and_then(|v| v.parse().ok())
        .expect("last_id");
    let landing = last + 60;
    assert_eq!(
        ask(
            &control,
            &format!("set {} exposure 5000 {landing}", cams[0])
        ),
        vec!["ok"]
    );

    // Each reader takes frames until it has seen the change and some frames after it.
    wait_until(
        "camera 0's reader sees the change",
        Duration::from_secs(60),
        || {
            for reader in readers.iter_mut() {
                reader.pump(Duration::from_millis(20));
            }
            let cam0 = readers[0].frames();
            cam0.iter().any(|f| f.1 == 5_000) && cam0.iter().filter(|f| f.1 == 5_000).count() >= 60
        },
    );
    for reader in readers.iter_mut() {
        reader.finish();
    }
    let lines: Vec<Vec<String>> = readers.iter().map(|r| r.lines_seen.clone()).collect();

    let cam0 = frame_lines(&lines[0]);
    let cam1 = frame_lines(&lines[1]);
    assert!(cam0.len() >= 100, "camera 0 frames read: {}", cam0.len());
    assert!(cam1.len() >= 100, "camera 1 frames read: {}", cam1.len());
    for (cam, frames) in [("camera 0", &cam0), ("camera 1", &cam1)] {
        assert!(
            frames.windows(2).all(|w| w[0].0 < w[1].0),
            "{cam}: ids in order"
        );
    }
    for line in lines.iter().flatten().filter(|l| l.starts_with("COPIES")) {
        assert_eq!(line, "COPIES 0", "the reader process copied frames");
    }
    assert_eq!(
        lines
            .iter()
            .flatten()
            .filter(|l| l.starts_with("COPIES"))
            .count(),
        2,
        "both readers finished and reported"
    );

    // The metadata the readers decoded: camera 0 changes exposure from the landing frame on,
    // camera 1 never does.
    assert!(
        cam1.iter().all(|f| f.1 == 10_000),
        "camera 1 exposure: {cam1:?}"
    );
    assert!(
        cam0.iter().all(|f| f.1 == 10_000 || f.1 == 5_000),
        "camera 0: {cam0:?}"
    );
    let first_changed = cam0.iter().find(|f| f.1 == 5_000).map(|f| f.0);
    let first_changed = first_changed.expect("camera 0's readers saw the exposure change");
    assert!(
        first_changed >= landing,
        "changed at {first_changed}, asked for {landing}"
    );
    assert!(
        cam0.iter()
            .filter(|f| f.0 < first_changed)
            .all(|f| f.1 == 10_000),
        "exposure is 10000 before the change"
    );

    // The bytes: each reader's checksum for a frame is the checksum the service read for it.
    let seen = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let mut matched = 0;
    for (cam, frames) in [(cams[0], &cam0), (cams[1], &cam1)] {
        for (id, _, checksum) in frames.iter() {
            if let Some(service) = seen.get(&(cam.to_string(), *id)) {
                assert_eq!(
                    service, checksum,
                    "{cam} frame {id}: the reader's bytes differ"
                );
                matched += 1;
            }
        }
    }
    assert!(matched >= 100, "matched {matched} frames by id");
    println!(
        "proof: readers_frames cam0={} cam1={} matched_bytes={matched} first_changed={first_changed} landing={landing} copies=0",
        cam0.len(),
        cam1.len()
    );
    let copies: usize = copy_counters().iter().map(|c| c.copies()).sum();
    assert_eq!(copies, 0, "the service copied frames");
    service.shutdown();
}

#[test]
fn a_camera_that_leaves_and_returns_is_read_again_by_its_reader() {
    let dir = test_dir("readers-return");
    let lemnos_socket = dir.join("lemnos.sock");
    let board = two_cameras();
    let lemnos = daemon(&lemnos_socket, &board);
    let text = format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\n\
         [camera.\"lemnos:cam0-sensor\"]\nfps_milli = 60000\n",
        dir.display(),
        lemnos_socket.display()
    );
    let recorder = Arc::new(Recorder::default());
    let config = Config::parse(&text).expect("config");
    let service =
        Service::start(config, deps(Arc::clone(&recorder), &lemnos_socket)).expect("start");
    let control = service.control_socket().to_path_buf();
    let cam = "lemnos:cam0-sensor";
    wait_until("streaming", Duration::from_secs(20), || {
        state_of(&control, cam) == "streaming"
    });
    let mut reader = ChildReader::spawn(&dir, &camera_socket(&dir, cam), "reader");
    wait_until("the reader has frames", Duration::from_secs(30), || {
        reader.pump(Duration::from_millis(20));
        reader.frames().len() >= 20
    });

    // The camera leaves: lemnosd goes away. The service retries with backoff and its producer stays
    // attached meanwhile: the reader is NOT closed while the camera retries, and the status says so.
    drop(lemnos);
    wait_until("the camera retries", Duration::from_secs(30), || {
        line_of(&control, cam)
            .and_then(|l| field(&l, "producer").map(|p| p.starts_with("retrying:")))
            .unwrap_or(false)
    });
    let retry_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < retry_deadline {
        reader.pump(Duration::from_millis(20));
    }
    assert!(
        !reader.has(|l| l == "CLOSED"),
        "the reader stays attached while the camera retries"
    );
    // It comes back on new buffers: the old ring ends (CLOSED) and the reader connects again.
    let _lemnos = daemon(&lemnos_socket, &board);
    wait_until("the reader sees the close", Duration::from_secs(60), || {
        reader.pump(Duration::from_millis(20));
        reader.has(|l| l == "CLOSED")
    });
    let before = reader.frames().len();

    // The reader connects again and takes frames from the new pool.
    wait_until("the reader connects again", Duration::from_secs(60), || {
        reader.pump(Duration::from_millis(20));
        reader
            .lines_seen
            .iter()
            .filter(|l| *l == "CONNECTED")
            .count()
            >= 2
    });
    wait_until("frames after the return", Duration::from_secs(60), || {
        reader.pump(Duration::from_millis(20));
        reader.frames().len() >= before + 30
    });
    let frames = reader.frames();
    let after: Vec<u64> = frames[before..].iter().map(|f| f.0).collect();
    assert!(
        after.windows(2).all(|w| w[0] < w[1]),
        "ids in order after the return"
    );
    let status = line_of(&control, cam).expect("listed");
    assert_eq!(field(&status, "publish"), Some("publishing"), "{status}");
    println!(
        "proof: reader_return frames_before_close={before} frames_after_return={} connects=2 copies=0",
        frames.len() - before
    );
    reader.finish();
    assert!(
        reader.has(|l| l == "COPIES 0"),
        "the reader reported copies: {:?}",
        reader.lines_seen.last()
    );
    service.shutdown();
}

// ---------------------------------------------------------------------------------------------
// A reader that never answers its handshake.

type Times = Arc<Mutex<Vec<(u64, Instant)>>>;

struct TimedOutlet(Times);

impl Outlet for TimedOutlet {
    fn frame(&mut self, _camera: &str, lease: &FrameLease<FrameMetadata>) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((lease.id(), Instant::now()));
    }
}

struct TimedOutlets(Times);

impl Outlets for TimedOutlets {
    fn attach(&self, _camera: &str) -> Box<dyn Outlet> {
        Box::new(TimedOutlet(Arc::clone(&self.0)))
    }
}

#[test]
fn a_reader_that_never_answers_does_not_stall_the_camera_and_is_dropped_at_the_timeout() {
    let dir = test_dir("stuck");
    let lemnos_socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&lemnos_socket, &two_cameras());
    let times: Times = Arc::default();
    let cam = "lemnos:cam0-sensor";
    let text = format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\ncameras = [\"{cam}\"]\n\
         [camera.\"{cam}\"]\nfps_milli = 60000\n",
        dir.display(),
        lemnos_socket.display()
    );
    let config = Config::parse(&text).expect("config");
    let mut deps = deps(Arc::new(Recorder::default()), &lemnos_socket);
    deps.outlets = Arc::new(TimedOutlets(Arc::clone(&times)));
    let service = Service::start(config, deps).expect("start the service");
    let control = service.control_socket().to_path_buf();
    wait_until("streaming", Duration::from_secs(20), || {
        state_of(&control, cam) == "streaming" && frames_of(&control, cam) > 50
    });

    // The stuck reader: it connects, is sent the offer, and never answers. It reads what it is
    // sent (so the offer does not just sit in a buffer) and waits to be closed.
    let socket = camera_socket(&dir, cam);
    let mut stuck = UnixStream::connect(&socket).expect("connect the stuck reader");
    stuck
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("timeout");
    let connected = Instant::now();
    let deadline = connected + Duration::from_secs(20);
    let mut scratch = [0_u8; 4096];
    let closed_at = loop {
        assert!(
            Instant::now() < deadline,
            "the stuck reader was never dropped"
        );
        if let Ok(0) = stuck.read(&mut scratch) {
            break Instant::now();
        }
    };
    let window = closed_at - connected;

    // The camera kept publishing through the whole handshake window: no gap between frames longer
    // than 250 ms (a stall of the handshake would be a gap of about the timeout, 5 s).
    let frames: Vec<(u64, Instant)> = times
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .copied()
        .filter(|(_, at)| *at >= connected && *at <= closed_at)
        .collect();
    let largest_gap = frames
        .windows(2)
        .map(|w| w[1].1 - w[0].1)
        .max()
        .unwrap_or(Duration::MAX);
    assert!(
        window >= Duration::from_secs(4),
        "the window was {window:?}"
    );
    assert!(
        largest_gap < Duration::from_millis(250),
        "the camera stalled for {largest_gap:?} around the stuck reader"
    );
    assert!(
        frames.windows(2).all(|w| w[1].0 > w[0].0),
        "frame ids in order"
    );
    // Dropped at the handshake timeout (5 s), not before and not much after.
    assert!(
        window >= Duration::from_millis(4_500) && window <= Duration::from_secs(10),
        "the stuck reader was dropped after {window:?}"
    );
    println!(
        "proof: stuck_reader window={window:?} frames_in_window={} largest_gap={largest_gap:?}",
        frames.len()
    );

    // A reader that does answer connects after it, and takes frames.
    let mut reader = Reader::connect(UnixStream::connect(&socket).expect("connect"))
        .expect("the handshake completes");
    let until = Instant::now() + Duration::from_secs(20);
    let mut taken = 0;
    while taken < 5 && Instant::now() < until {
        match reader.next_frame().expect("not cut off") {
            Some(_) => taken += 1,
            None => {
                let _ = reader.wait(Some(Duration::from_millis(5)));
            }
        }
    }
    assert!(
        taken >= 5,
        "the reader after the stuck one got {taken} frames"
    );
    let status = line_of(&control, cam).expect("listed");
    assert_eq!(field(&status, "publish"), Some("publishing"), "{status}");
    service.shutdown();
}
