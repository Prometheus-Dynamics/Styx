//! The 3A loop (`aaa = true`) on the simulated backend: `lemnosd` in this process with the paced
//! camera (its sensor and receiver on one frame clock, so a posted value lands on the frame its
//! delay names) and the simulated PiSP back end, served by the service.
//!
//! The simulated frames are black, so AE always wants more light: an automatic value that landed
//! after a manual one would show as a brighter exposure or gain than the one set. Each case has its
//! own daemon and service; the cases run in order in one test (the back end's input counter and
//! this binary's allocation count are process-wide).

use styx_sim::paced as sim;
use styx_sim::pisp_be;

use std::alloc::{GlobalAlloc, Layout, System};
use std::backtrace::Backtrace;
use std::cell::Cell;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config as DaemonConfig, Service as LemnosDaemon};
use styx_controls::FrameMetadata;
use styx_native_run::SystemClock;
use styx_select::listing::{LibcameraLister, ListError, Listed, V4l2Lister};
use styx_service::{Config, Deps, Discard, Listers, Service, camera_socket, isp_socket};
use styx_socket::Reader;

// Allocations on the camera's ISP thread (named `styx-isp:<camera>`), for its steady state.
thread_local! {
    static ISP_THREAD: Cell<u8> = const { Cell::new(0) };
    static IN_ALLOC: Cell<bool> = const { Cell::new(false) };
}
static ISP_ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
// While set, each ISP-thread allocation is also recorded (TODO Q15): its size, the frame the test
// was taking (FRAME_INDEX) and where it came from, printed when the check fails. Recording happens
// under IN_ALLOC, so its own allocations are not counted; it is on only for the checked window.
static RECORD: AtomicBool = AtomicBool::new(false);
static FRAME_INDEX: AtomicU64 = AtomicU64::new(0);
static RECORDED: Mutex<Vec<Recorded>> = Mutex::new(Vec::new());

struct Recorded {
    what: &'static str,
    size: usize,
    frame: u64,
    backtrace: Backtrace,
}

fn record(what: &'static str, size: usize) {
    let _ = IN_ALLOC.try_with(|busy| {
        busy.set(true);
        let entry = Recorded {
            what,
            size,
            frame: FRAME_INDEX.load(Ordering::Relaxed),
            backtrace: Backtrace::force_capture(),
        };
        RECORDED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(entry);
        busy.set(false);
    });
}

fn count(what: &'static str, size: usize) {
    if IN_ALLOC.try_with(Cell::get).unwrap_or(true) {
        return;
    }
    let isp = ISP_THREAD
        .try_with(|state| {
            if state.get() == 0 {
                let named = IN_ALLOC.try_with(|busy| {
                    busy.set(true);
                    let named = std::thread::current()
                        .name()
                        .is_some_and(|n| n.starts_with("styx-isp:"));
                    busy.set(false);
                    named
                });
                state.set(if named.unwrap_or(false) { 1 } else { 2 });
            }
            state.get() == 1
        })
        .unwrap_or(false);
    if isp {
        ISP_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        if RECORD.load(Ordering::Relaxed) {
            record(what, size);
        }
    }
}

struct Counting;

// SAFETY: every call forwards to the system allocator with the same arguments; the only addition
// is a count kept in const-initialised thread-locals and an atomic, which do not allocate, and,
// while RECORD is set, a record made with IN_ALLOC set (its own allocations reach `alloc` again,
// return at once from `count`, and go to the system allocator).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count("alloc", layout.size());
        // SAFETY: the caller upholds GlobalAlloc's contract for `layout`; forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `alloc` above with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count("realloc", new_size);
        // SAFETY: `ptr` came from `alloc` above with this `layout`; forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const CAMERA: &str = "lemnos:cam0-sensor";

#[derive(Debug)]
struct Nothing;

impl V4l2Lister for Nothing {
    fn list_v4l2(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

impl LibcameraLister for Nothing {
    fn list_libcamera(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    _daemon: LemnosDaemon,
    service: Option<Service>,
    control: PathBuf,
    socket_dir: PathBuf,
}

impl Rig {
    fn start(camera_keys: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("styx-aaa-")
            .tempdir()
            .expect("a scratch folder");
        let lemnos = dir.path().join("lemnosd.sock");
        let board = sim::board_from(sim::Board {
            sensor_link: Some("csi0"),
            pisp_be: true,
            receiver_buffers: Some(12),
            camera: Some("aaa-cam"),
            ..sim::Board::default()
        });
        let daemon = LemnosDaemon::start(DaemonConfig {
            socket: lemnos.clone(),
            name: "styx-aaa-test".to_string(),
            board: parse(&board).expect("board"),
            drivers: vec![
                entry::<sim::SimSensor>(),
                entry::<sim::SimReceiver>(),
                entry::<pisp_be::SimPisp>(),
            ],
            backend: Box::new(SimBoard::new()),
            options: Options {
                call_deadline: Duration::from_millis(200),
                ..Options::default()
            },
        })
        .expect("start lemnosd");
        let socket_dir = dir.path().join("styx");
        // A full module tuning (DPC, denoise, ALSC, ...), as a board runs, for a camera that names
        // it; the others find nothing for their name here and run the defaults.
        let tuning = dir.path().join("tuning");
        std::fs::create_dir(&tuning).expect("the tuning folder");
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../data/tuning/ov9782.toml"
            ),
            tuning.join("full.toml"),
        )
        .expect("the full tuning");
        let text = format!(
            "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\ncameras = [\"{CAMERA}\"]\nscan_ms = 100\n\
             tuning_dirs = [\"{}\"]\n\
             [camera.\"{CAMERA}\"]\nwidth = 320\nheight = 40\nfps_milli = 60000\n\
             exposure_us = 1000\ngain_milli = 1000\naaa = true\n{camera_keys}",
            socket_dir.display(),
            lemnos.display(),
            tuning.display(),
        );
        let service = Service::start(
            Config::parse(&text).expect("config"),
            Deps {
                clock: Arc::new(SystemClock::default()),
                listers: Listers {
                    native: Arc::new(styx_service::Daemon { socket: lemnos }),
                    libcamera: Arc::new(Nothing),
                    v4l2: Arc::new(Nothing),
                },
                outlets: Arc::new(Discard),
            },
        )
        .expect("start the service");
        let control = service.control_socket().to_path_buf();
        Self {
            _dir: dir,
            _daemon: daemon,
            service: Some(service),
            control,
            socket_dir,
        }
    }

    fn line(&self) -> String {
        ask(&self.control, "list")
            .into_iter()
            .find(|l| l.starts_with(&format!("camera {CAMERA} ")))
            .unwrap_or_default()
    }

    fn wait_for(&self, what: &str, ok: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let line = self.line();
            if ok(&line) {
                return line;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {line}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn reader(&self, path: &Path) -> Reader {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(stream) = UnixStream::connect(path)
                && let Ok(reader) = Reader::connect(stream)
            {
                return reader;
            }
            assert!(Instant::now() < deadline, "no reader on {}", path.display());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn raw_reader(&self) -> Reader {
        self.reader(&camera_socket(&self.socket_dir, CAMERA))
    }

    fn isp_reader(&self) -> Reader {
        self.reader(&isp_socket(&self.socket_dir, CAMERA, 0))
    }

    fn set(&self, control: &str, value: &str, frame: u64) {
        let answer = ask(
            &self.control,
            &format!("set {CAMERA} {control} {value} {frame}"),
        );
        assert_eq!(
            answer.last().map(String::as_str),
            Some("ok"),
            "set {control} {value}: {answer:?}"
        );
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(service) = self.service.take() {
            service.shutdown();
        }
    }
}

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

/// One raw frame: its id and the exposure and gain its metadata says it was made with.
#[derive(Debug, Clone, Copy)]
struct Shot {
    id: u64,
    exposure_us: u32,
    gain_milli: u32,
}

fn shot(reader: &mut Reader) -> Shot {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(frame) = reader.next_frame().expect("the raw stream stays up") {
            let meta = FrameMetadata::from_bytes(frame.user()).expect("the frame's metadata");
            return Shot {
                id: frame.lease().id(),
                exposure_us: meta.exposure_us,
                gain_milli: meta.analogue_gain_milli,
            };
        }
        assert!(Instant::now() < deadline, "no frame within 5 s");
        let _ = reader.wait(Some(Duration::from_millis(5)));
    }
}

/// Raw frames until one at or past `id`, then `more` frames after it.
fn shots_through(reader: &mut Reader, id: u64, more: usize) -> Vec<Shot> {
    let mut got = Vec::new();
    let mut after = 0;
    while after < more {
        let s = shot(reader);
        if s.id >= id {
            after += 1;
        }
        got.push(s);
    }
    got
}

#[test]
fn the_3a_loop_on_the_simulated_camera() {
    manual_values_win_from_their_frame_and_auto_gives_them_back();
    pisp_back_end_counts_statistics_in_its_input_pass_and_allocates_nothing();
    a_full_tuning_stays_on_the_back_end_and_names_what_it_leaves_out();
}

/// `isp = "auto"` with a full module tuning stays on the back end: the setting the back end has
/// no published values for (DPC) is left out and named, never a reason to fall to the soft ISP
/// (the first 3A board proof did, and the soft ISP then made every other frame). Every frame
/// asked for is counted against those made.
fn a_full_tuning_stays_on_the_back_end_and_names_what_it_leaves_out() {
    pisp_be::refuse_next(0);
    pisp_be::refuse_outputs(false);
    let rig = Rig::start("isp = \"auto\"\ntuning = \"full.toml\"\n");
    let line = rig.wait_for("the loop on the back end with the full tuning", |l| {
        field(l, "isp_3a") == Some("ae,awb") && field(l, "isp_left_out") != Some("-")
    });
    assert!(
        field(&line, "aaa_tuning").is_some_and(|t| t.contains("/full.toml")),
        "{line}"
    );
    let mut reader = rig.isp_reader();
    for _ in 0..60 {
        let _ = take(&mut reader);
    }
    let line = rig.line();
    println!("proof: full tuning on auto: {line}");
    assert_eq!(field(&line, "isp"), Some("pisp"), "{line}");
    assert_eq!(field(&line, "isp_fallback"), Some("-"), "{line}");
    assert!(
        field(&line, "isp_switches").is_some_and(|s| s.starts_with("0/")),
        "never switched to the soft ISP: {line}"
    );
    assert!(
        field(&line, "isp_left_out").is_some_and(|t| t.starts_with("denoise.dpc_(")),
        "DPC named as left out: {line}"
    );
    let (made, asked) = field(&line, "isp_made")
        .and_then(|v| v.split_once('/'))
        .map(|(a, b)| (a.parse::<u64>().unwrap(), b.parse::<u64>().unwrap()))
        .expect("isp_made=<made>/<asked>");
    assert!(made >= 60 && asked >= made, "{line}");
}

/// The soft ISP's statistics drive AE on black frames: exposure and gain rise. A manual exposure set
/// for frame F is on every frame from F (AE, still wanting more light, never lands a value after
/// it); a manual gain likewise; `auto` gives exposure back and AE raises it again. The loop's
/// controls are listed and set by name, and the status says what runs.
fn manual_values_win_from_their_frame_and_auto_gives_them_back() {
    let rig = Rig::start("isp = \"soft\"\n");
    let line = rig.wait_for("the loop running", |l| field(l, "isp_3a") == Some("ae,awb"));
    assert_eq!(field(&line, "flicker"), Some("off"));
    // The statistics' sampling as it runs: one frame in 4 (the default), and one quad row in at
    // most 4 (fewer on a frame too short for every zone row to have one).
    let (every, rows) = field(&line, "aaa_stats")
        .and_then(|v| v.split_once('/'))
        .map(|(a, b)| (a.parse::<u32>().unwrap(), b.parse::<u32>().unwrap()))
        .expect("aaa_stats=<every>/<rows>");
    assert!(every == 4 && (1..=4).contains(&rows), "{line}");
    assert!(
        field(&line, "aaa_tuning").is_some_and(|t| t.contains("defaults") || t.contains("generic")),
        "the tuning is named: {line}"
    );
    // AE brightens a black scene from the configured 1000 us at 1.0x.
    let line = rig.wait_for("AE raising the exposure or gain", |l| {
        field(l, "exposure_requested").and_then(|v| v.parse::<u64>().ok()) > Some(1000)
            || field(l, "gain_requested").and_then(|v| v.parse::<u64>().ok()) > Some(1000)
    });
    println!("proof: AE on black frames: {line}");

    let controls = ask(&rig.control, &format!("controls {CAMERA}"));
    for name in [
        "ae",
        "ev",
        "awb",
        "red_gain",
        "blue_gain",
        "colour_temperature",
        "flicker",
    ] {
        assert!(
            controls
                .iter()
                .any(|l| l.starts_with(&format!("control {name} "))),
            "{name} is listed: {controls:?}"
        );
    }
    let exposure = controls
        .iter()
        .find(|l| l.starts_with("control exposure "))
        .expect("exposure");
    assert_eq!(field(exposure, "auto"), Some("true"));
    assert_eq!(field(exposure, "value"), Some("auto"));

    // Manual exposure from frame F, below what AE wants.
    let mut raw = rig.raw_reader();
    let now = shot(&mut raw).id;
    let from = now + 30;
    rig.set("exposure", "3000", from);
    let shots = shots_through(&mut raw, from, 60);
    let held: Vec<&Shot> = shots.iter().filter(|s| s.id >= from).collect();
    let first = held.first().expect("frames from F").exposure_us;
    assert!(
        first.abs_diff(3000) <= 10,
        "the manual exposure, to the line: {first}"
    );
    assert!(
        held.iter().all(|s| s.exposure_us == first),
        "every frame from F has the manual exposure, none of AE's: {held:?}"
    );
    let before: Vec<&Shot> = shots.iter().filter(|s| s.id < from).collect();
    // No frame before F has the manual value (the switch is frame-exact): each has AE's, or the
    // configured start on frames before AE's first value landed.
    assert!(
        before.iter().all(|s| s.exposure_us != first),
        "the manual exposure on a frame before F: {before:?}"
    );
    assert!(
        before.last().is_some_and(|s| s.exposure_us > 3000),
        "AE's brighter exposure on the frame before F: {before:?}"
    );
    println!(
        "proof: manual exposure 3000 from frame {from}: {} frames from it all at {first} us; before it {:?}",
        held.len(),
        before.last().map(|s| s.exposure_us)
    );

    // Manual gain too: AE has nothing left to move.
    let now = shot(&mut raw).id;
    let from_gain = now + 30;
    rig.set("gain", "1500", from_gain);
    let shots = shots_through(&mut raw, from_gain, 60);
    let held: Vec<&Shot> = shots.iter().filter(|s| s.id >= from_gain).collect();
    assert!(
        held.iter()
            .all(|s| s.gain_milli.abs_diff(1500) <= 63 && s.gain_milli == held[0].gain_milli),
        "every frame from F has the manual gain: {held:?}"
    );
    assert!(held.iter().all(|s| s.exposure_us == first), "{held:?}");
    rig.wait_for("only AWB left automatic", |l| {
        field(l, "isp_3a") == Some("awb")
    });

    // Exposure back to the loop: AE raises it again past the manual value.
    rig.set("exposure", "auto", 0);
    let line = rig.wait_for("AE moving exposure again", |l| {
        field(l, "exposure_requested").and_then(|v| v.parse::<u64>().ok()) > Some(3100)
    });
    assert_eq!(field(&line, "isp_3a"), Some("ae,awb"));

    // The loop's own controls by name; refused values say why.
    rig.set("ae", "off", 0);
    rig.set("ev", "-1.5", 0);
    rig.set("flicker", "50", 0);
    rig.set("red_gain", "1800", 0);
    let line = rig.wait_for("AE and AWB held", |l| {
        field(l, "isp_3a") == Some("manual") && field(l, "flicker") == Some("50")
    });
    assert_eq!(field(&line, "ae_state"), Some("-"), "{line}");
    let refused = ask(&rig.control, &format!("set {CAMERA} flicker 55 0"));
    assert!(
        refused
            .last()
            .is_some_and(|l| l.starts_with("err flicker takes")),
        "{refused:?}"
    );
}

/// The back end: the loop's statistics come from the input pass (no more bytes read or written per
/// frame than without it), and once AE has settled the ISP thread allocates nothing per frame.
fn pisp_back_end_counts_statistics_in_its_input_pass_and_allocates_nothing() {
    pisp_be::refuse_next(0);
    pisp_be::refuse_outputs(false);
    let rig = Rig::start("isp = \"auto\"\n");
    rig.wait_for("the loop on the back end", |l| {
        field(l, "isp") == Some("pisp") && field(l, "isp_3a") == Some("ae,awb")
    });
    let mut reader = rig.isp_reader();
    // Long enough for AE to reach its limits on black frames and settle.
    for _ in 0..120 {
        let _ = take(&mut reader);
    }
    let line = rig.wait_for("AE settled", |l| field(l, "ae_state") == Some("settled"));
    let copy = |rig: &Rig| {
        ask(&rig.control, "list")
            .iter()
            .filter_map(|l| styx_service::control::parse_copy_line(l))
            .find(|c| c.site == "pisp-be-input")
            .expect("the pisp-be-input copy line")
    };
    let before = copy(&rig);
    RECORDED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    FRAME_INDEX.store(0, Ordering::Relaxed);
    RECORD.store(true, Ordering::Relaxed);
    let before_alloc = ISP_ALLOCATIONS.load(Ordering::Relaxed);
    for i in 0..60 {
        FRAME_INDEX.store(i, Ordering::Relaxed);
        let _ = take(&mut reader);
    }
    let after_alloc = ISP_ALLOCATIONS.load(Ordering::Relaxed);
    RECORD.store(false, Ordering::Relaxed);
    let after = copy(&rig);
    let passes = after.copies - before.copies;
    // The paced camera's first mode: 320x40 RAW10 in, 16-bit out; nothing more for the statistics.
    assert_eq!(after.bytes - before.bytes, passes * 320 * 2 * 40);
    assert_eq!(
        after.bytes_read.unwrap_or(0) - before.bytes_read.unwrap_or(0),
        passes * 320 * 5 / 4 * 40
    );
    let allocations = after_alloc - before_alloc;
    println!(
        "proof: back end with 3A: passes={passes} isp_thread_allocations={allocations} {}",
        field(&line, "wb_gains").unwrap_or("-")
    );
    if allocations != 0 {
        // What allocated, for TODO Q15: the frame is the index of the frame the test was waiting
        // for when the ISP thread allocated (0-59 of the checked window).
        let recorded = RECORDED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        eprintln!(
            "the ISP thread's allocations in the checked window: {} recorded",
            recorded.len()
        );
        for (n, r) in recorded.iter().enumerate() {
            eprintln!(
                "allocation {n}: {} of {} bytes while waiting for frame {}\n{}",
                r.what, r.size, r.frame, r.backtrace
            );
        }
    }
    assert_eq!(allocations, 0, "the ISP thread allocates nothing per frame");
}

fn take(reader: &mut Reader) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(frame) = reader.next_frame().expect("the processed stream stays up") {
            return frame.lease().id();
        }
        assert!(Instant::now() < deadline, "no frame within 5 s");
        let _ = reader.wait(Some(Duration::from_millis(5)));
    }
}
