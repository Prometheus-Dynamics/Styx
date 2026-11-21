//! The processed stream (`isp`) on the simulated backend: `lemnosd` in this process with the paced
//! camera and the simulated PiSP back end (`styx_sim::pisp_be`) after its receiver, served by the
//! service, read on `<camera socket>.isp0.sock` by a reader in this process.
//!
//! Its own binary and one test running the cases in order: the simulated back end's knobs
//! (refusals, a refused geometry) and the back end's input counter are process-wide. Each case has
//! its own daemon and service. The policy's clock is the test's (a manual clock), so the switch,
//! the retry delays and their cap are stepped, not waited for.
//!
//! Not run here: a lent input (an FE upstream). The paced chain has no front end, so the copy path
//! is the one that runs; the lend path is proven in styx-pisp's `live_sim` lend cases.

use styx_sim::paced as sim;
use styx_sim::pisp_be;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config as DaemonConfig, Service as LemnosDaemon};
use styx_controls::MadeBy;
use styx_format::FourCc;
use styx_native_run::Clock;
use styx_select::listing::{LibcameraLister, ListError, Listed, V4l2Lister};
use styx_service::{Config, Deps, Discard, Listers, Service, camera_socket, isp_socket};
use styx_socket::{IspActive, ReadError, Reader};

// Allocations are counted per thread, and summed for the camera's ISP thread (named
// `styx-isp:<camera>`) so its steady state can be asserted.
thread_local! {
    static ISP_THREAD: Cell<u8> = const { Cell::new(0) };
    static IN_ALLOC: Cell<bool> = const { Cell::new(false) };
}
static ISP_ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

fn on_isp_thread() -> bool {
    ISP_THREAD
        .try_with(|state| {
            if state.get() == 0 {
                // Looked up once per thread, with this counter off while it runs.
                let isp = IN_ALLOC.try_with(|busy| {
                    busy.set(true);
                    let named = std::thread::current()
                        .name()
                        .is_some_and(|n| n.starts_with("styx-isp:"));
                    busy.set(false);
                    named
                });
                state.set(if isp.unwrap_or(false) { 1 } else { 2 });
            }
            state.get() == 1
        })
        .unwrap_or(false)
}

fn count() {
    if IN_ALLOC.try_with(Cell::get).unwrap_or(true) {
        return;
    }
    if on_isp_thread() {
        ISP_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

struct Counting;

// SAFETY: every call forwards to the system allocator with the same arguments; the only addition
// is a count kept in const-initialised thread-locals and an atomic, which do not allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller upholds GlobalAlloc's contract for `layout`; forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator with `layout`; forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: `ptr` and `layout` are the caller's, and `new_size` is the caller's; forwarded.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const CAMERA: &str = "lemnos:cam0-sensor";
/// The paced camera's first mode: 320x40 CSI-2 RAW10.
const WIDTH: u64 = 320;
const HEIGHT: u64 = 40;
/// Bytes the back end's input pass reads and writes per frame: RAW10 rows (400 bytes) in, 16-bit
/// rows (640 bytes) out.
const READ_PER_FRAME: u64 = WIDTH * 5 / 4 * HEIGHT;
const WRITTEN_PER_FRAME: u64 = WIDTH * 2 * HEIGHT;

/// The policy's clock: moved by the test. A backoff wait returns after a short real pause.
#[derive(Default)]
struct Manual {
    now: Mutex<Duration>,
}

impl Manual {
    fn advance(&self, by: Duration) {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner) += by;
    }
}

impl Clock for Manual {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait(&self, _for: Duration, _cancel: &AtomicBool) {
        std::thread::sleep(Duration::from_millis(2));
    }
}

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

/// One case's daemon, service and folder.
struct Rig {
    _dir: tempfile::TempDir,
    _daemon: LemnosDaemon,
    service: Option<Service>,
    control: PathBuf,
    socket_dir: PathBuf,
    clock: Arc<Manual>,
}

impl Rig {
    /// The rig with a receiver pool roomy enough for the ISP's share at a hand-off of 2.
    fn start(camera_keys: &str) -> Self {
        Self::start_with(camera_keys, 12)
    }

    /// The rig with `buffers` in the receiver's pool.
    fn start_with(camera_keys: &str, buffers: u32) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("styx-isp-")
            .tempdir()
            .expect("a scratch folder");
        let lemnos = dir.path().join("lemnosd.sock");
        let board = sim::board_from(sim::Board {
            sensor_link: Some("csi0"),
            pisp_be: true,
            receiver_buffers: Some(buffers),
            ..sim::Board::default()
        });
        let daemon = LemnosDaemon::start(DaemonConfig {
            socket: lemnos.clone(),
            name: "styx-isp-test".to_string(),
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
        let text = format!(
            "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\ncameras = [\"{CAMERA}\"]\nscan_ms = 100\n\
             [camera.\"{CAMERA}\"]\nwidth = 320\nheight = 40\nfps_milli = 120000\n{camera_keys}",
            socket_dir.display(),
            lemnos.display()
        );
        let clock = Arc::new(Manual::default());
        let service = Service::start(
            Config::parse(&text).expect("config"),
            Deps {
                clock: clock.clone(),
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
            clock,
        }
    }

    fn line(&self) -> String {
        ask(&self.control, "list")
            .into_iter()
            .find(|l| l.starts_with(&format!("camera {CAMERA} ")))
            .unwrap_or_default()
    }

    fn copy_line(&self) -> Option<styx_service::control::CopyLine> {
        ask(&self.control, "list")
            .iter()
            .filter_map(|l| styx_service::control::parse_copy_line(l))
            .find(|c| c.site == "pisp-be-input")
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

    fn isp_reader(&self, k: usize) -> Reader {
        let path = isp_socket(&self.socket_dir, CAMERA, k);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(stream) = UnixStream::connect(&path)
                && let Ok(reader) = Reader::connect(stream)
            {
                return reader;
            }
            assert!(Instant::now() < deadline, "no reader on {}", path.display());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn raw_reader(&self) -> Reader {
        let path = camera_socket(&self.socket_dir, CAMERA);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(stream) = UnixStream::connect(&path)
                && let Ok(reader) = Reader::connect(stream)
            {
                return reader;
            }
            assert!(Instant::now() < deadline, "no raw reader");
            std::thread::sleep(Duration::from_millis(20));
        }
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

fn number(line: &str, key: &str) -> u64 {
    field(line, key)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("{key} in {line}"))
}

/// One frame a reader took: its raw frame's id, what made it, and its format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Took {
    id: u64,
    made_by: MadeBy,
    code: FourCc,
    width: u32,
    height: u32,
}

/// The next frame on `reader`, waiting up to five seconds.
fn take(reader: &mut Reader) -> Result<Took, ReadError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(frame) = reader.next_frame()? {
            let lease = frame.lease();
            let format = lease.format();
            return Ok(Took {
                id: lease.id(),
                made_by: MadeBy::of(frame.user()).expect("a metadata blob with its made-by byte"),
                code: format.code,
                width: format.resolution.width.get(),
                height: format.resolution.height.get(),
            });
        }
        assert!(Instant::now() < deadline, "no frame within 5 s");
        let _ = reader.wait(Some(Duration::from_millis(5)));
    }
}

/// Frames until `stop` says enough (at most `max`), in order.
fn take_while(reader: &mut Reader, max: usize, stop: impl Fn(&[Took]) -> bool) -> Vec<Took> {
    let mut got = Vec::new();
    while got.len() < max && !stop(&got) {
        got.push(take(reader).expect("the processed stream stays up"));
    }
    got
}

/// The largest run of raw ids missing between consecutive frames.
fn largest_gap(frames: &[Took]) -> u64 {
    frames
        .windows(2)
        .map(|w| w[1].id.saturating_sub(w[0].id + 1))
        .max()
        .unwrap_or(0)
}

fn reset_back_end() {
    pisp_be::refuse_next(0);
    pisp_be::refuse_outputs(false);
    pisp_be::clear_ran();
}

#[test]
fn the_processed_stream_on_the_simulated_back_end() {
    back_end_frames_with_the_copy_counted_and_none_allocated();
    reset_back_end();
    refusals_switch_to_soft_with_no_gap_and_tries_double_to_the_cap_then_switch_back();
    reset_back_end();
    a_geometry_the_back_end_refuses_starts_on_soft_and_pisp_alone_has_no_stream();
    reset_back_end();
    readers_on_both_sockets_and_raw_each_keep_their_own_reserve();
    reset_back_end();
    nine_buffers_run_the_isp_at_a_hand_off_of_one_and_leave_raw_readers_a_frame();
    reset_back_end();
    eight_buffers_refuse_the_isp_by_name_and_the_raw_stream_runs();
}

/// Case 1: `auto` with a back end. Frames on isp0 are the back end's, in output 0's format and
/// size; the input pass is counted with the bytes it read and wrote; the ISP thread allocates
/// nothing per frame once running.
fn back_end_frames_with_the_copy_counted_and_none_allocated() {
    let rig = Rig::start("isp = \"auto\"\n");
    let line = rig.wait_for("isp=pisp", |l| field(l, "isp") == Some("pisp"));
    assert_eq!(field(&line, "isp_asked"), Some("auto"));
    assert_eq!(field(&line, "isp_form"), Some("raw10-unpack-to-bayer16"));
    assert_eq!(
        field(&line, "isp_3a"),
        Some("none"),
        "no 3A: a stated limit"
    );
    let mut reader = rig.isp_reader(0);
    let warm = take_while(&mut reader, 10, |_| false);
    assert!(warm.iter().all(|t| t.made_by == MadeBy::Pisp), "{warm:?}");
    assert!(
        warm.iter()
            .all(|t| (t.code, t.width, t.height) == (FourCc::NV12, 320, 40)),
        "output 0's format and size: {warm:?}"
    );
    let state = reader.isp();
    assert_eq!(
        state.active,
        IspActive::Pisp,
        "the state page names the ISP"
    );

    // Steady state: the ISP thread's allocations over 60 frames.
    let before_alloc = ISP_ALLOCATIONS.load(Ordering::Relaxed);
    let before = rig.copy_line().expect("the pisp-be-input copy line");
    let start = rig.line();
    let start = field(&start, "isp_published").unwrap_or("-").to_string();
    let steady = take_while(&mut reader, 60, |_| false);
    let after_alloc = ISP_ALLOCATIONS.load(Ordering::Relaxed);
    let after = rig.copy_line().expect("the pisp-be-input copy line");
    let line = rig.line();
    let passes = after.copies - before.copies;
    // A pass per frame the ISP made. The counter and the reader are read at different moments
    // (frames already in the ring when `before` was read are taken in the 60), so the two agree
    // within the ring's window; the bytes per pass are exact.
    assert!(
        passes.abs_diff(steady.len() as u64) <= 8,
        "a pass per frame: {passes} passes, {} frames",
        steady.len()
    );
    assert_eq!(
        after.bytes - before.bytes,
        passes * WRITTEN_PER_FRAME,
        "bytes written"
    );
    assert_eq!(
        after.bytes_read.expect("bytes_read") - before.bytes_read.expect("bytes_read"),
        passes * READ_PER_FRAME,
        "bytes read"
    );
    assert!(largest_gap(&steady) <= 1, "{steady:?}");
    let allocations = after_alloc - before_alloc;
    println!(
        "proof: case 1 frames={} passes={passes} read/frame={READ_PER_FRAME} written/frame={WRITTEN_PER_FRAME} \
         isp_thread_allocations={allocations} published {start}->{} skipped={}",
        steady.len(),
        field(&line, "isp_published").unwrap_or("-"),
        number(&line, "isp_skipped"),
    );
    assert_eq!(allocations, 0, "the ISP thread allocates nothing per frame");
}

/// Case 3 with the switch rules: three refusals in a row switch to the soft ISP (each refused
/// frame made by the soft ISP, so the isp0 reader sees no gap from them); the back end is tried
/// after the first delay, refused tries double it to the cap, and the first completed try switches
/// back, on the same producer (the reader stays connected).
fn refusals_switch_to_soft_with_no_gap_and_tries_double_to_the_cap_then_switch_back() {
    let rig = Rig::start(
        "isp = \"auto\"\nisp_refusals = 3\nisp_retry_start_ms = 100\nisp_retry_cap_ms = 400\n",
    );
    rig.wait_for("isp=pisp", |l| field(l, "isp") == Some("pisp"));
    let mut reader = rig.isp_reader(0);
    let mut seen = take_while(&mut reader, 5, |_| false);
    assert!(seen.iter().all(|t| t.made_by == MadeBy::Pisp));

    pisp_be::refuse_next(3);
    // Three refused jobs, then frames on the soft ISP: the clock does not move, so no try.
    seen.extend(take_while(&mut reader, 200, |got| {
        got.iter().filter(|t| t.made_by == MadeBy::Soft).count() >= 6
    }));
    let line = rig.wait_for("the switch", |l| field(l, "isp") == Some("soft"));
    assert_eq!(number(&line, "isp_refused"), 3, "{line}");
    assert_eq!(field(&line, "isp_switches"), Some("1/0"), "{line}");
    assert!(
        field(&line, "isp_reason").is_some_and(|r| r.contains("bad-config")),
        "the reason is readable: {line}"
    );
    assert_ne!(
        field(&line, "isp_reason_at"),
        Some("-"),
        "with its time: {line}"
    );
    let page = reader.isp();
    assert_eq!(
        (page.active, page.to_soft, page.refused),
        (IspActive::Soft, 1, 3)
    );
    assert!(page.reason().contains("bad-config"), "{}", page.reason());

    // Tries: every one refused while refusals are left. The delay doubles 100 -> 200 -> 400 and
    // holds at the cap.
    pisp_be::refuse_next(u64::MAX);
    let ms = Duration::from_millis;
    for (refused, (delay, label)) in (3..).zip([
        (100, "first"),
        (200, "doubled"),
        (400, "doubled to the cap"),
        (400, "held at the cap"),
    ]) {
        // Short of the delay: no try.
        rig.clock.advance(ms(delay - 1));
        seen.extend(take_while(&mut reader, 4, |_| false));
        assert_eq!(
            number(&rig.line(), "isp_refused"),
            refused,
            "{label}: no try before {delay} ms"
        );
        rig.clock.advance(ms(1));
        rig.wait_for(label, |l| number(l, "isp_refused") == refused + 1);
        seen.extend(take_while(&mut reader, 4, |_| false));
    }
    assert_eq!(
        field(&rig.line(), "isp_switches"),
        Some("1/0"),
        "tries are not switches"
    );

    // The back end takes jobs again: the next try completes and the camera switches back.
    pisp_be::refuse_next(0);
    rig.clock.advance(ms(400));
    seen.extend(take_while(&mut reader, 300, |got| {
        got.iter()
            .rev()
            .take(5)
            .filter(|t| t.made_by == MadeBy::Pisp)
            .count()
            == 5
    }));
    let line = rig.wait_for("the switch back", |l| field(l, "isp") == Some("pisp"));
    assert_eq!(field(&line, "isp_switches"), Some("1/1"), "{line}");
    assert_eq!(reader.isp().to_pisp, 1);

    // What the isp0 reader saw, in order: back end, soft (the refused frames and the delay), back
    // end, on one connection. At each switch the frames are consecutive or one apart (the rule).
    // Anywhere, a missing frame is accounted for: skipped for the ISP (counted), skipped by the
    // raw stream, or passed by this reader itself when the test thread was late (its own count).
    let runs = seen
        .windows(2)
        .filter(|w| w[0].made_by != w[1].made_by)
        .count();
    assert_eq!(runs, 2, "pisp -> soft -> pisp: {:?}", made_by_runs(&seen));
    let at_switch = seen
        .windows(2)
        .filter(|w| w[0].made_by != w[1].made_by)
        .map(|w| w[1].id.saturating_sub(w[0].id + 1))
        .max()
        .unwrap_or(0);
    assert!(
        at_switch <= 1,
        "gap {at_switch} at a switch: {:?}",
        gaps(&seen)
    );
    let missing: u64 = seen
        .windows(2)
        .map(|w| w[1].id.saturating_sub(w[0].id + 1))
        .sum();
    let line = rig.line();
    let counted = number(&line, "isp_skipped") + number(&line, "skipped") + reader.stats().skipped;
    assert!(
        missing <= counted,
        "{missing} frames missing, {counted} counted: {:?} {line}",
        gaps(&seen)
    );
    println!(
        "proof: case 3 frames={} runs={:?} largest_gap={} at_switch={at_switch} missing={missing} \
         reader_skipped={} gaps>1={:?} {line}",
        seen.len(),
        made_by_runs(&seen),
        largest_gap(&seen),
        reader.stats().skipped,
        gaps(&seen),
    );
}

/// Each gap larger than one frame: the frames either side, with what made them.
fn gaps(frames: &[Took]) -> Vec<(u64, &'static str, u64, &'static str)> {
    frames
        .windows(2)
        .filter(|w| w[1].id > w[0].id + 2)
        .map(|w| (w[0].id, w[0].made_by.name(), w[1].id, w[1].made_by.name()))
        .collect()
}

/// What made consecutive runs of frames, with their lengths.
fn made_by_runs(frames: &[Took]) -> Vec<(&'static str, usize)> {
    let mut runs: Vec<(&'static str, usize)> = Vec::new();
    for t in frames {
        match runs.last_mut() {
            Some((name, n)) if *name == t.made_by.name() => *n += 1,
            _ => runs.push((t.made_by.name(), 1)),
        }
    }
    runs
}

/// Case 2 and 4: a back end that refuses the geometry at open. With `auto` the soft ISP runs from
/// the first frame with the refusal as the reason, and once the back end takes the geometry a try
/// opens it and the camera switches to it (the producers are made again over its buffers, so the
/// reader connects again). With `pisp` there is no processed stream, the reason is shown, and the
/// raw socket is unaffected.
fn a_geometry_the_back_end_refuses_starts_on_soft_and_pisp_alone_has_no_stream() {
    pisp_be::refuse_outputs(true);
    {
        let rig = Rig::start("isp = \"auto\"\nisp_retry_start_ms = 100\n");
        let line = rig.wait_for("isp=soft", |l| field(l, "isp") == Some("soft"));
        assert!(
            field(&line, "isp_reason").is_some_and(|r| r.contains("set_output")),
            "the back end's refusal is the reason: {line}"
        );
        let mut reader = rig.isp_reader(0);
        let frames = take_while(&mut reader, 10, |_| false);
        assert!(
            frames.iter().all(|t| t.made_by == MadeBy::Soft),
            "{frames:?}"
        );
        pisp_be::refuse_outputs(false);
        rig.clock.advance(Duration::from_millis(100));
        let line = rig.wait_for("the back end opened", |l| field(l, "isp") == Some("pisp"));
        assert_eq!(field(&line, "isp_switches"), Some("0/1"), "{line}");
        // The pool changed: this reader's stream ends, and a new reader gets the back end's frames.
        let ended = (0..200).find_map(|_| take(&mut reader).err());
        println!("proof: case 2 the reader at the reopen saw {ended:?}");
        let mut again = rig.isp_reader(0);
        let frames = take_while(&mut again, 5, |_| false);
        assert!(
            frames.iter().all(|t| t.made_by == MadeBy::Pisp),
            "{frames:?}"
        );
    }
    pisp_be::refuse_outputs(true);
    {
        let rig = Rig::start("isp = \"pisp\"\n");
        let line = rig.wait_for("the reason", |l| {
            field(l, "isp_reason").is_some_and(|r| r.contains("set_output"))
        });
        assert_eq!(field(&line, "isp"), Some("none"), "{line}");
        // The raw stream runs.
        let mut raw = rig.raw_reader();
        assert!(take(&mut raw).is_ok());
        let before = number(&rig.line(), "frames");
        rig.wait_for("raw frames", |l| number(l, "frames") > before + 10);
        // A reader on isp0 is closed at its handshake: nothing is published.
        let path = isp_socket(&rig.socket_dir, CAMERA, 0);
        let stream = UnixStream::connect(&path).expect("the socket exists");
        assert!(
            Reader::connect(stream).is_err(),
            "no processed stream to read"
        );
        assert_eq!(field(&rig.line(), "isp_published"), Some("0/0"));
    }
}

/// Case 6: two readers on isp0, one on isp1 and one on raw all take frames; each producer has its
/// own pool and reserve.
fn readers_on_both_sockets_and_raw_each_keep_their_own_reserve() {
    let rig = Rig::start("isp = \"auto\"\nisp_second = \"NV12:160x20\"\n");
    rig.wait_for("isp=pisp", |l| field(l, "isp") == Some("pisp"));
    let mut a = rig.isp_reader(0);
    let mut b = rig.isp_reader(0);
    let mut second = rig.isp_reader(1);
    let mut raw = rig.raw_reader();
    for _ in 0..20 {
        let (x, y) = (take(&mut a).expect("a"), take(&mut b).expect("b"));
        assert_eq!((x.width, y.width), (320, 320));
        let s = take(&mut second).expect("isp1");
        assert_eq!((s.code, s.width, s.height), (FourCc::NV12, 160, 20));
        assert!(take(&mut raw).is_ok());
    }
    let line = rig.line();
    assert_eq!(field(&line, "isp_readers"), Some("2/1"), "{line}");
    println!("proof: case 6 {line}");
}

/// Case 7: nine receiver buffers. The native hold keeps 5, so the ISP runs at a hand-off of 1
/// (2 raw frames), the raw producer's reserve is 7, and a raw reader may hold 1 older frame while
/// it takes the next.
fn nine_buffers_run_the_isp_at_a_hand_off_of_one_and_leave_raw_readers_a_frame() {
    let rig = Rig::start_with("isp = \"auto\"\n", 9);
    rig.wait_for("isp=pisp", |l| field(l, "isp") == Some("pisp"));
    let line = rig.wait_for("buffers=9", |l| field(l, "buffers") == Some("9"));
    assert_eq!(field(&line, "reserve"), Some("7"), "{line}");
    let mut isp = rig.isp_reader(0);
    let mut raw = rig.raw_reader();
    let held = next_raw(&mut raw);
    for _ in 0..30 {
        let newer = next_raw(&mut raw);
        assert!(newer.lease().id() > held.lease().id());
        drop(newer);
        assert_eq!(take(&mut isp).expect("isp0 keeps up").made_by, MadeBy::Pisp);
    }
    drop(held);
    let line = rig.line();
    assert_eq!(
        field(&line, "reserve_cuts"),
        Some("0"),
        "the held frame was never cut: {line}"
    );
    println!("proof: case 7 {line}");
}

/// Case 8: eight receiver buffers. The native hold keeps 5; the ISP's 2 would leave raw readers
/// nothing, so `isp` is refused by name and the raw stream runs at its own reserve.
fn eight_buffers_refuse_the_isp_by_name_and_the_raw_stream_runs() {
    let rig = Rig::start_with("isp = \"auto\"\n", 8);
    let line = rig.wait_for("isp refused", |l| {
        field(l, "isp_reason")
            .is_some_and(|r| r.starts_with("isp_refused:_the_camera's_pool_has_8_buffers"))
    });
    assert_eq!(field(&line, "isp"), Some("none"), "{line}");
    let mut raw = rig.raw_reader();
    for _ in 0..20 {
        next_raw(&mut raw);
    }
    let line = rig.line();
    assert_eq!(field(&line, "buffers"), Some("8"), "{line}");
    assert_eq!(field(&line, "reserve"), Some("5"), "{line}");
    assert_eq!(field(&line, "isp_published"), Some("0/0"), "{line}");
    println!("proof: case 8 {line}");
}

/// The next raw frame, held, waiting up to five seconds.
fn next_raw(reader: &mut Reader) -> styx_socket::Frame {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(frame) = reader.next_frame().expect("the raw stream stays up") {
            return frame;
        }
        assert!(Instant::now() < deadline, "no raw frame within 5 s");
        std::thread::sleep(Duration::from_millis(2));
    }
}
