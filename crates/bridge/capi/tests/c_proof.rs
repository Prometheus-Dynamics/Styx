//! The C proof: `tests/c/proof.c`, built with the system `cc` against `libstyx_capi.a`, runs over
//! both paths against the simulated camera. In process: a Lemnos daemon on a socket, the C program
//! opens the camera through `styx-select`. Over the socket: `styx-service` serves the same daemon,
//! and the C program is its client (control socket and the camera's frame socket).
//!
//! Skips with a printed reason when there is no C compiler.

use styx_sim::drivers::{SimIsp, SimReceiver, SimSensor};

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config as DaemonConfig, Service as LemnosDaemon};
use styx_native_run::SystemClock;
use styx_select::listing::{LibcameraLister, ListError, Listed, V4l2Lister};
use styx_service::{Config, Daemon, Deps, Discard, Listers, Service};

/// The camera the proof opens: the first of the two simulated cameras.
const CAMERA: &str = "lemnos:cam0-sensor";

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
    let dir = std::env::temp_dir().join(format!("styx-capi-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("test dir");
    dir
}

fn daemon(socket: &Path) -> LemnosDaemon {
    let config = DaemonConfig {
        socket: socket.to_path_buf(),
        name: "styx-capi-test".to_string(),
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
    LemnosDaemon::start(config).expect("start lemnosd")
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

/// The C compiler, if there is one.
fn cc() -> Option<&'static str> {
    let ok = Command::new("cc")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    ok.then_some("cc")
}

/// `libstyx_capi.a` in this build's profile directory, built (or brought up to date) through cargo
/// first: an integration test links the rlib, and the static library is a build product of this
/// package. Cargo finds it current when nothing changed.
fn staticlib() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    let profile = exe
        .parent()
        .and_then(Path::parent)
        .expect("the target profile directory")
        .to_path_buf();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(cargo)
        .current_dir(&root)
        .args(["build", "-p", "styx-capi", "--lib"])
        .status()
        .expect("cargo build styx-capi");
    assert!(status.success(), "cargo build -p styx-capi --lib");
    let lib = profile.join("libstyx_capi.a");
    assert!(lib.exists(), "{} is built", lib.display());
    lib
}

/// Compiles the proof with the header's sizes checked against the Rust structs.
fn build_proof(dir: &Path, cc: &str, lib: &Path) -> PathBuf {
    let include = Path::new(env!("CARGO_MANIFEST_DIR")).join("include");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/c/proof.c");
    let exe = dir.join("proof");
    let sizes = [
        format!(
            "-DSTYX_EXPECT_CONTROL_VALUE={}",
            std::mem::size_of::<styx_capi::StyxControlValue>()
        ),
        format!(
            "-DSTYX_EXPECT_OPEN_OPTIONS={}",
            std::mem::size_of::<styx_capi::StyxOpenOptions>()
        ),
        format!(
            "-DSTYX_EXPECT_FRAME_INFO={}",
            std::mem::size_of::<styx_capi::FrameInfo>()
        ),
        format!(
            "-DSTYX_EXPECT_PLANE={}",
            std::mem::size_of::<styx_capi::PlaneOut>()
        ),
        format!(
            "-DSTYX_EXPECT_COUNTER_SNAPSHOT={}",
            std::mem::size_of::<styx_capi::CounterSnapshot>()
        ),
    ];
    let output = Command::new(cc)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-O1"])
        .arg("-I")
        .arg(&include)
        .args(&sizes)
        .arg(&source)
        .arg(lib)
        .args(["-lpthread", "-ldl", "-lm", "-o"])
        .arg(&exe)
        .output()
        .expect("run cc");
    assert!(
        output.status.success(),
        "cc failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    exe
}

/// Runs the proof and returns its stdout. Fails on a non-zero exit or after the timeout.
fn run_proof(exe: &Path, mode: &str, socket: &Path) -> String {
    let mut child = Command::new(exe)
        .arg(mode)
        .arg(socket)
        .arg(CAMERA)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the proof");
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        if let Some(status) = child.try_wait().expect("poll the proof") {
            let mut out = String::new();
            let mut err = String::new();
            use std::io::Read;
            if let Some(mut s) = child.stdout.take() {
                let _ = s.read_to_string(&mut out);
            }
            if let Some(mut s) = child.stderr.take() {
                let _ = s.read_to_string(&mut err);
            }
            assert!(status.success(), "proof {mode} failed:\n{out}\n{err}");
            return out;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("proof {mode} did not finish");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_ascii_whitespace().find_map(|word| {
        word.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('='))
    })
}

/// Waits (up to 20 seconds) until the service's own `list` shows `name` with a frame socket. The
/// service scans on its own schedule, so a listing made right after it starts can come back empty.
fn wait_until_listed(control: &Path, name: &str) {
    use std::io::{BufRead, BufReader, Write};
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let listed = std::os::unix::net::UnixStream::connect(control).and_then(|mut stream| {
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            writeln!(stream, "list")?;
            let mut text = String::new();
            for line in BufReader::new(stream).lines() {
                let line = line?;
                if line == "ok" || line.starts_with("err") {
                    break;
                }
                text.push_str(&line);
                text.push('\n');
            }
            Ok(text)
        });
        if let Ok(text) = listed
            && text
                .lines()
                .any(|l| l.starts_with(&format!("camera {name} ")) && l.contains("socket=/"))
        {
            return;
        }
        assert!(Instant::now() < deadline, "the service never listed {name}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Checks the PROOF line the C program printed.
fn check_proof(out: &str, mode: &str) -> String {
    let line = out
        .lines()
        .find(|l| l.starts_with("PROOF "))
        .unwrap_or_else(|| panic!("no PROOF line:\n{out}"));
    assert!(line.starts_with(&format!("PROOF {mode} ")), "{line}");
    assert_eq!(field(line, "frames"), Some("200"), "{line}");
    assert_eq!(field(line, "ordered"), Some("1"), "{line}");
    assert_eq!(field(line, "copies"), Some("0"), "{line}");
    assert_eq!(field(line, "copy_bytes"), Some("0"), "{line}");
    assert_eq!(field(line, "exposure_read_back"), Some("5000"), "{line}");
    assert_eq!(field(line, "descriptor"), Some("1"), "{line}");
    assert_eq!(field(line, "memory_kind"), Some("1"), "{line}");
    assert_eq!(
        field(line, "meta_bytes").map(|v| v != "0"),
        Some(true),
        "{line}"
    );
    line.to_string()
}

/// Reads the stdout's `LISTED` line too, so a listing is part of the record.
fn listed(out: &str) -> String {
    out.lines()
        .find(|l| l.starts_with("LISTED "))
        .unwrap_or_default()
        .to_string()
}

#[test]
fn a_c_program_opens_the_simulated_camera_in_process_and_gets_ordered_frames() {
    let Some(cc) = cc() else {
        println!("skipped: no C compiler (cc) on this machine");
        return;
    };
    let dir = test_dir("local");
    let socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&socket);
    let lib = staticlib();
    let exe = build_proof(&dir, cc, &lib);
    let out = run_proof(&exe, "local", &socket);
    println!("{}", listed(&out));
    println!("{}", check_proof(&out, "local"));
}

#[test]
fn a_c_program_opens_the_same_camera_over_the_service_socket_and_gets_ordered_frames() {
    let Some(cc) = cc() else {
        println!("skipped: no C compiler (cc) on this machine");
        return;
    };
    let dir = test_dir("service");
    let lemnos_socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&lemnos_socket);
    let lib = staticlib();
    let exe = build_proof(&dir, cc, &lib);

    let text = format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\n\
         [camera.\"{CAMERA}\"]\nfps_milli = 60000\nexposure_us = 10000\n",
        dir.display(),
        lemnos_socket.display()
    );
    let deps = Deps {
        clock: Arc::new(SystemClock::default()),
        listers: Listers {
            native: Arc::new(Daemon {
                socket: lemnos_socket.clone(),
            }),
            libcamera: Arc::new(NoV4l2),
            v4l2: Arc::new(NoV4l2),
        },
        outlets: Arc::new(Discard),
    };
    let service = Service::start(Config::parse(&text).expect("config"), deps).expect("service");
    let control = service.control_socket().to_path_buf();
    wait_until_listed(&control, CAMERA);
    let out = run_proof(&exe, "service", &control);
    println!("{}", listed(&out));
    println!("{}", check_proof(&out, "service"));
    // The frames the service served were not copied either: its own counters stay at zero.
    let copies: u64 = styx_frame::copy_counters()
        .iter()
        .map(|c| c.copies() as u64)
        .sum();
    assert_eq!(copies, 0, "the service copied frames");
    service.shutdown();
}
