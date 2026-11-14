//! `styx-measure read` on a camera that stalls and comes back (styx-sim's `SimRig`, served by the
//! service in this process): the stall and the return are printed and counted, the time across the
//! stall is reported as `stall, <ms>`, and no frame is counted lost from it. A Lost return is a
//! discontinuity.
//!
//! Every wait has a deadline and fails saying what it waited for.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use styx_native_run::SystemClock;
use styx_select::listing::{LibcameraLister, ListError, Listed, V4l2Lister};
use styx_service::{Config, Deps, Discard, Listers, Service, camera_socket};
use styx_sim::{Fault, SimRig};

const CAM: &str = "lemnos:cam0-sensor";
const REASON: &str = "bus error while streaming: restarting the sensor";
const WAIT: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct NoLister;

impl V4l2Lister for NoLister {
    fn list_v4l2(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

impl LibcameraLister for NoLister {
    fn list_libcamera(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

fn test_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("styx-measure-stall-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("test dir");
    dir
}

/// Sends one request to the control socket and reads its lines through `ok` or `err`.
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

/// The camera's status line, if the service lists it streaming.
fn streaming(control: &Path) -> bool {
    ask(control, "list")
        .iter()
        .any(|l| l.starts_with(&format!("camera {CAM} ")) && l.contains(" state=streaming "))
}

fn run_measure(socket: &Path, report: &Path, seconds: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_styx-measure"))
        .args([
            "read",
            "--socket",
            &socket.display().to_string(),
            "--seconds",
            seconds,
            "--report",
            &report.display().to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run styx-measure")
}

/// Waits for the child to end (within `WAIT`) and returns its stdout and stderr.
fn finish(mut child: Child) -> (String, String) {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().expect("poll styx-measure") {
            assert!(status.success(), "styx-measure failed: {status}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "waited {WAIT:?} for styx-measure to end"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut stdout = String::new();
    let mut stderr = String::new();
    use std::io::Read;
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    (stdout, stderr)
}

fn json_u64(v: &serde_json::Value, path: &[&str]) -> u64 {
    let mut cur = v;
    for key in path {
        cur = cur.get(*key).unwrap_or_else(|| panic!("missing {path:?}"));
    }
    cur.as_u64()
        .unwrap_or_else(|| panic!("{path:?} is not a number: {cur}"))
}

fn json_f64s(v: &serde_json::Value, path: &[&str]) -> Vec<f64> {
    let mut cur = v;
    for key in path {
        cur = cur.get(*key).unwrap_or_else(|| panic!("missing {path:?}"));
    }
    cur.as_array()
        .unwrap_or_else(|| panic!("{path:?} is not an array: {cur}"))
        .iter()
        .map(|x| x.as_f64().unwrap_or_else(|| panic!("{path:?}: {x}")))
        .collect()
}

/// A rig and the service on it, with the camera streaming.
fn start(name: &str) -> (SimRig, Service, PathBuf) {
    let dir = test_dir(name);
    let rig = SimRig::start().expect("the simulated camera");
    let text = format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\n\
         [camera.\"{CAM}\"]\nfps_milli = 60000\n",
        dir.display(),
        rig.socket().display()
    );
    let deps = Deps {
        clock: Arc::new(SystemClock::default()),
        listers: Listers {
            native: Arc::new(styx_service::Daemon {
                socket: rig.socket().to_path_buf(),
            }),
            libcamera: Arc::new(NoLister),
            v4l2: Arc::new(NoLister),
        },
        outlets: Arc::new(Discard),
    };
    let service = Service::start(Config::parse(&text).expect("config"), deps).expect("start");
    let control = service.control_socket().to_path_buf();
    let deadline = Instant::now() + WAIT;
    while !streaming(&control) {
        assert!(
            Instant::now() < deadline,
            "waited {WAIT:?} for the camera to stream"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    (rig, service, dir)
}

/// Reads the camera's frames for two seconds, a stall of about a second, then the return; the
/// report and the edges printed by the tool are what the test checks.
#[test]
fn a_stall_and_its_return_are_printed_counted_and_not_counted_as_lost_frames() {
    let (rig, service, dir) = start("kept");
    let socket = camera_socket(&dir, CAM);
    let report = dir.join("kept.json");
    let child = run_measure(&socket, &report, "6");
    std::thread::sleep(Duration::from_secs(2));
    rig.inject_fault(Some(Fault::Failed(REASON)));
    std::thread::sleep(Duration::from_millis(1000));
    rig.inject_fault(None);
    let (stdout, stderr) = finish(child);
    service.shutdown();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).expect("the report")).expect("json");
    assert_eq!(json_u64(&json, &["frames", "stalls"]), 1, "{stderr}");
    assert_eq!(
        json_u64(&json, &["frames", "producer_resumes"]),
        1,
        "{stderr}"
    );
    assert_eq!(
        json_u64(&json, &["frames", "discontinuities"]),
        0,
        "{stderr}"
    );
    let stall_ms = json_f64s(&json, &["frames", "stall_gaps_ms"]);
    assert_eq!(
        stall_ms.len(),
        1,
        "one time gap across the one stall: {stall_ms:?}"
    );
    assert!(
        stall_ms[0] >= 500.0,
        "the gap is the stall, about a second: {stall_ms:?}"
    );
    let total = json
        .get("frames")
        .and_then(|f| f.get("stall_ms_total"))
        .and_then(|v| v.as_f64())
        .expect("stall_ms_total");
    assert!(total >= 500.0, "the stall's time is counted: {total}");
    assert!(stderr.contains("producer stalled:"), "{stderr}");
    assert!(
        stderr.contains("producer resumed:") && stderr.contains("kept: same driver instance"),
        "{stderr}"
    );
    assert!(
        stdout.contains("stall, "),
        "the table names the stall: {stdout}"
    );
    println!(
        "proof: measure_kept stall_gap_ms={:.3} stall_ms_total={total:.3}",
        stall_ms[0]
    );
}

/// The device is bound again while it is down: the return is Lost, a discontinuity of the sequence.
#[test]
fn a_lost_return_is_a_discontinuity_and_not_a_lost_frame() {
    let (rig, service, dir) = start("lost");
    let socket = camera_socket(&dir, CAM);
    let report = dir.join("lost.json");
    let child = run_measure(&socket, &report, "6");
    std::thread::sleep(Duration::from_secs(2));
    rig.inject_fault(Some(Fault::Gone(REASON)));
    std::thread::sleep(Duration::from_millis(1000));
    rig.inject_fault(None);
    let (stdout, stderr) = finish(child);
    service.shutdown();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).expect("the report")).expect("json");
    assert_eq!(json_u64(&json, &["frames", "stalls"]), 1, "{stderr}");
    assert_eq!(
        json_u64(&json, &["frames", "discontinuities"]),
        1,
        "{stderr}"
    );
    assert_eq!(json_f64s(&json, &["frames", "stall_gaps_ms"]).len(), 1);
    assert!(stderr.contains("lost: rebound, a new sequence"), "{stderr}");
    assert!(stdout.contains("stall, "), "{stdout}");
    println!("proof: measure_lost discontinuities=1 stalls=1");
}
