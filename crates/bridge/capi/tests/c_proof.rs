//! The C surface over both paths against the simulated camera. In process: a Lemnos daemon on a
//! socket, the camera opened through `styx-select`. Over the socket: `styx-service` serves the
//! same daemon, and the caller is its client (control socket and the camera's frame socket).
//!
//! Each path is run twice. `tests/c/proof.c`, the minimal C caller, is built with the system `cc`
//! against `libstyx_capi.a`: the header compiles, its structs are the library's sizes, and a C
//! program opens, reads frames in order, sees an error status and closes. Then [`deep`] calls the
//! same entry points from Rust for everything else: a control landing on its frame, planes,
//! descriptors, the metadata record, the counters decoder and the copy counters.
//!
//! The C half does not run, with the reason recorded, when there is no C compiler; the Rust half
//! always runs.

use styx_sim::paced as sim;

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

/// `libstyx_capi.a` with the package's own default features, built (or brought up to date) through
/// cargo first: an integration test links the rlib, and the static library is a build product of
/// this package. A C program links it with libc alone; a build of the whole workspace's features
/// would pull in their native libraries (libcamera's C++ among them).
///
/// It is built in its own target directory, `<profile>/c-proof`: a library with a C crate type
/// keeps one file name whatever its features, so built beside the workspace's build it would
/// overwrite the workspace's styx-capi and every later build would link it again. Cargo finds it
/// current when nothing changed.
fn staticlib() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    let profile = exe
        .parent()
        .and_then(Path::parent)
        .expect("the target profile directory")
        .to_path_buf();
    let target = profile.join("c-proof");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(cargo)
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", &target)
        .args(["build", "-p", "styx-capi", "--lib"])
        .status()
        .expect("cargo build styx-capi");
    assert!(status.success(), "cargo build -p styx-capi --lib");
    let lib = target.join("debug").join("libstyx_capi.a");
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
    assert_eq!(field(line, "frames"), Some("30"), "{line}");
    assert_eq!(field(line, "ordered"), Some("1"), "{line}");
    line.to_string()
}

/// Builds and runs the C caller on `socket`, when there is a C compiler.
fn c_caller(dir: &Path, mode: &str, socket: &Path) {
    let Some(cc) = cc() else {
        styx_frame::not_run::not_run("cc", "C caller", "no C compiler (cc) on this machine");
        return;
    };
    let exe = build_proof(dir, cc, &staticlib());
    println!("{}", check_proof(&run_proof(&exe, mode, socket), mode));
}

const FRAMES: usize = 200;
const WAIT_MS: u32 = 5_000;

fn ok(status: i32, what: &str) {
    assert_eq!(status, styx_capi::OK, "{what}");
}

/// The next frame and its description; the caller releases it.
fn next(camera: *mut styx_capi::Camera) -> (*mut styx_capi::Frame, styx_capi::FrameInfo) {
    let mut frame = std::ptr::null_mut();
    let mut info = std::mem::MaybeUninit::<styx_capi::FrameInfo>::zeroed();
    // SAFETY: `camera` is the live handle the caller opened; the out-pointers are valid for the
    // calls, and `info` is written whole by a call that returns OK.
    unsafe {
        ok(
            styx_capi::styx_camera_next_frame(camera, WAIT_MS, &mut frame),
            "a frame arrives within the wait",
        );
        ok(
            styx_capi::styx_frame_describe(frame, info.as_mut_ptr()),
            "styx_frame_describe",
        );
        (frame, info.assume_init())
    }
}

/// The header's `STYX_METADATA_BYTES`.
fn header_metadata_bytes() -> usize {
    let header = Path::new(env!("CARGO_MANIFEST_DIR")).join("include/styx.h");
    std::fs::read_to_string(header)
        .expect("styx.h")
        .lines()
        .find_map(|l| l.strip_prefix("#define STYX_METADATA_BYTES "))
        .and_then(|v| v.trim().parse().ok())
        .expect("STYX_METADATA_BYTES in styx.h")
}

/// The deep checks, through the C entry points: the camera is listed and opened by name, an
/// exposure set for a frame ahead shows from that frame and reads back, 200 frames come in order
/// with their plane, descriptor and metadata record, the counters decoder refuses bad records, and
/// nothing was copied.
fn deep(mode: &str, socket: &Path) {
    use std::os::unix::ffi::OsStrExt;
    use styx_capi::*;
    let path = socket.as_os_str().as_bytes();
    // SAFETY: every pointer passed below is to a live local or a handle this function opened and
    // has not closed; lengths are the buffers' own. Frames are released once, handles closed once.
    unsafe {
        let mut source = std::ptr::null_mut();
        let opened = match mode {
            "local" => styx_source_local(path.as_ptr(), path.len(), &mut source),
            _ => styx_source_service(path.as_ptr(), path.len(), &mut source),
        };
        ok(opened, "open the source");

        let mut count = 0;
        ok(styx_source_camera_count(source, &mut count), "camera count");
        let listed = (0..count).any(|i| {
            let mut name = [0_u8; 256];
            let mut len = 0;
            ok(
                styx_source_camera_name(source, i, name.as_mut_ptr(), name.len(), &mut len),
                "camera name",
            );
            &name[..len] == CAMERA.as_bytes()
        });
        assert!(listed, "{mode}: the named camera is listed among {count}");

        let options = StyxOpenOptions {
            width: 0,
            height: 0,
            fps_milli: 60_000,
            exposure_us: 10_000,
            gain_milli: 1_000,
        };
        let mut camera = std::ptr::null_mut();
        ok(
            styx_camera_open(source, CAMERA.as_ptr(), CAMERA.len(), &options, &mut camera),
            "open the camera",
        );

        let mut last = 0;
        for i in 0..10 {
            let (frame, info) = next(camera);
            assert!(i == 0 || info.id > last, "warm-up ids rise");
            last = info.id;
            styx_frame_release(frame);
        }

        // Exposure: 5000 us from a frame a few ahead; it shows from there, within the sensor's
        // delay, and reads back as set.
        let target = last + 30;
        let exposure = StyxControlValue {
            kind: VALUE_MANUAL,
            value: 5_000,
        };
        ok(
            styx_camera_set_control(camera, b"exposure".as_ptr(), 8, exposure, target),
            "set exposure",
        );
        let mut seen_from = None;
        for _ in 0..400 {
            let (frame, info) = next(camera);
            assert!(info.id > last, "ids rise while waiting for the exposure");
            last = info.id;
            styx_frame_release(frame);
            if info.exposure_us == 5_000 {
                seen_from = Some(info.id);
                break;
            }
        }
        let seen_from = seen_from.expect("exposure 5000 shows on a frame");
        assert!(
            (target..=target + 12).contains(&seen_from),
            "{mode}: exposure set for {target} shows from {seen_from}"
        );
        let mut read_back = StyxControlValue { kind: 0, value: 0 };
        ok(
            styx_camera_get_control(camera, b"exposure".as_ptr(), 8, &mut read_back),
            "get exposure",
        );
        assert_eq!((read_back.kind, read_back.value), (VALUE_MANUAL, 5_000));

        // The metadata record's size is the library's, and the header states the same.
        let mut need = 0;
        ok(styx_frame_metadata_size(&mut need), "metadata size");
        assert_eq!(need, header_metadata_bytes());
        let mut record = vec![0_u8; need];

        let (mut first, mut gaps, mut size) = (0, 0, (0, 0));
        for i in 0..FRAMES {
            let (frame, info) = next(camera);
            if i == 0 {
                first = info.id;
                size = (info.width, info.height);
                // The buffer as a descriptor: the simulated camera's frames are a memfd.
                let (mut fd, mut offset, mut len, mut kind) = (-1, 0, 0, 0);
                ok(
                    styx_frame_fd(frame, &mut fd, &mut offset, &mut len, &mut kind),
                    "styx_frame_fd",
                );
                assert!(fd >= 0 && len > 0, "a descriptor names the buffer");
                assert_eq!((kind, info.memory_kind), (MEMORY_MEMFD, MEMORY_MEMFD));
            } else {
                assert!(info.id > last, "frame ids are strictly in order");
                gaps += info.id - last - 1;
            }
            last = info.id;
            assert_eq!((info.width, info.height), size, "frame size is steady");
            assert!(info.exposure_us >= 1, "frames carry an exposure");

            let mut plane = PlaneOut {
                data: std::ptr::null(),
                len: 0,
                stride: 0,
            };
            ok(styx_frame_plane(frame, 0, &mut plane), "plane 0");
            assert!(!plane.data.is_null() && plane.len > 0 && plane.stride > 0);
            assert_eq!(
                styx_frame_plane(frame, info.planes, &mut plane),
                E_NOT_FOUND,
                "a plane past the last is not found"
            );

            // A buffer of the record's size holds it whole; one byte short is refused with the
            // length still reported.
            let mut len = 0;
            ok(
                styx_frame_metadata(frame, record.as_mut_ptr(), need, &mut len),
                "metadata",
            );
            assert_eq!(len, need);
            let mut short = 0;
            assert_eq!(
                styx_frame_metadata(frame, record.as_mut_ptr(), need - 1, &mut short),
                E_INVALID
            );
            assert_eq!(short, need, "a short buffer reports the length");
            styx_frame_release(frame);
        }
        assert!(last > first, "the ids moved on");

        // The counters decoder refuses a short record and a record of another layout by status.
        let mut counters = std::mem::MaybeUninit::<CounterSnapshot>::zeroed();
        assert_eq!(
            styx_counters_read(b"short".as_ptr(), 5, counters.as_mut_ptr()),
            E_INVALID
        );
        let junk = [0xab_u8; 714];
        assert_eq!(
            styx_counters_read(junk.as_ptr(), junk.len(), counters.as_mut_ptr()),
            E_PROTOCOL
        );

        let (mut copies, mut copy_bytes) = (1, 1);
        ok(styx_copies(&mut copies, &mut copy_bytes), "styx_copies");
        assert_eq!((copies, copy_bytes), (0, 0), "no frame bytes were copied");

        println!(
            "DEEP {mode} frames={FRAMES} first_id={first} last_id={last} ordered=1 gaps={gaps} \
             exposure_set_from={target} exposure_seen_from={seen_from} exposure_read_back=5000 \
             size={}x{} meta_bytes={need} memory_kind=1 descriptor=1 copies=0",
            size.0, size.1
        );
        styx_camera_close(camera);
        styx_source_close(source);
    }
}

#[test]
fn the_c_surface_opens_the_simulated_camera_in_process_and_gets_ordered_frames() {
    let dir = test_dir("local");
    let socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&socket);
    c_caller(&dir, "local", &socket);
    deep("local", &socket);
}

#[test]
fn the_c_surface_opens_the_same_camera_over_the_service_socket_and_gets_ordered_frames() {
    let dir = test_dir("service");
    let lemnos_socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&lemnos_socket);

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
    c_caller(&dir, "service", &control);
    deep("service", &control);
    // The frames the service served were not copied either: its own counters stay at zero.
    let copies: u64 = styx_frame::copy_counters()
        .iter()
        .map(|c| c.copies() as u64)
        .sum();
    assert_eq!(copies, 0, "the service copied frames");
    service.shutdown();
}
