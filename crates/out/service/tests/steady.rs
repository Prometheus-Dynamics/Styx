//! The service's frame path allocates nothing and copies nothing, in steady state, with a
//! simulated camera on lemnosd.
//!
//! A counting global allocator keeps a count per thread. The camera's outlet runs on the camera's
//! own thread, once per frame, and records how many allocations that thread made since the frame
//! before: the stream's next frame, the status update and the outlet call all sit in that window.

mod sim;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config as DaemonConfig, Service as LemnosDaemon};
use styx_controls::FrameMetadata;
use styx_frame::FrameLease;
use styx_native_run::SystemClock;
use styx_select::listing::{LibcameraLister, ListError, Listed, V4l2Lister};
use styx_service::{Config, Deps, Listers, Outlet, Outlets, Service};

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn allocations() -> u64 {
    ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
}

fn count() {
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
}

struct Counting;

// SAFETY: every call forwards to the system allocator with the same arguments; the only addition
// is a thread-local counter, which does not allocate (it is const-initialised).
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

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The counts seen by the outlet, shared with the test. Plain atomics: the outlet allocates
/// nothing on the frame path.
#[derive(Default)]
struct Tally {
    frames: AtomicU64,
    /// Frames whose window (since the previous frame) had at least one allocation.
    windows_with_allocations: AtomicU64,
    /// The largest allocation count in any one window.
    worst_window: AtomicU64,
    /// Frames seen after the warm-up, the ones the steady-state claim is about.
    steady_frames: AtomicU64,
    steady_allocations: AtomicU64,
}

struct Probe {
    tally: Arc<Tally>,
    last: Option<u64>,
}

const WARM_UP: u64 = 50;

impl Outlet for Probe {
    fn frame(&mut self, _camera: &str, lease: &FrameLease<FrameMetadata>) {
        let now = allocations();
        let n = self.tally.frames.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(last) = self.last {
            let window = now - last;
            if window > 0 {
                self.tally
                    .windows_with_allocations
                    .fetch_add(1, Ordering::Relaxed);
                self.tally.worst_window.fetch_max(window, Ordering::Relaxed);
            }
            if n > WARM_UP {
                self.tally.steady_frames.fetch_add(1, Ordering::Relaxed);
                self.tally
                    .steady_allocations
                    .fetch_add(window, Ordering::Relaxed);
            }
        }
        // The lease is released when this returns. Its id is the frame's identity.
        let _ = lease.id();
        self.last = Some(allocations());
    }
}

struct Probes(Arc<Tally>);

impl Outlets for Probes {
    fn attach(&self, _camera: &str) -> Box<dyn Outlet> {
        Box::new(Probe {
            tally: Arc::clone(&self.0),
            last: None,
        })
    }
}

#[derive(Debug)]
struct NoOthers;

impl LibcameraLister for NoOthers {
    fn list_libcamera(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

impl V4l2Lister for NoOthers {
    fn list_v4l2(&self) -> Result<Vec<Listed>, ListError> {
        Ok(Vec::new())
    }
}

const BOARD: &str = "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-cameras\"\n\
     [[devices]]\nid = \"isp0\"\ndriver = \"sim-isp\"\n\
     [[devices]]\nid = \"csi0\"\ndriver = \"sim-receiver\"\nlink = \"isp0\"\n\
     [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
     [[devices]]\nid = \"cam0-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi0\"\n";

fn daemon(socket: &std::path::Path) -> LemnosDaemon {
    let config = DaemonConfig {
        socket: socket.to_path_buf(),
        name: "styx-service-steady".to_string(),
        board: parse(BOARD).expect("board"),
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

#[test]
fn the_frame_path_allocates_and_copies_nothing_in_steady_state() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("styx-service-steady-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let socket = dir.join("lemnos.sock");
    let _lemnos = daemon(&socket);

    let tally = Arc::new(Tally::default());
    let text = format!(
        "socket_dir = \"{}\"\nlemnos_socket = \"{}\"\nscan_ms = 100\ncameras = [\"lemnos:cam0-sensor\"]\n\
         [camera.\"lemnos:cam0-sensor\"]\nfps_milli = 60000\n",
        dir.display(),
        socket.display()
    );
    let config = Config::parse(&text).expect("config");
    let deps = Deps {
        clock: Arc::new(SystemClock::default()),
        listers: Listers {
            native: Arc::new(styx_service::Daemon {
                socket: socket.clone(),
            }),
            libcamera: Arc::new(NoOthers),
            v4l2: Arc::new(NoOthers),
        },
        outlets: Arc::new(Probes(Arc::clone(&tally))),
    };
    let service = Service::start(config, deps).expect("start the service");

    let deadline = Instant::now() + Duration::from_secs(30);
    while tally.frames.load(Ordering::Relaxed) < 1_000 {
        assert!(
            Instant::now() < deadline,
            "only {} frames",
            tally.frames.load(Ordering::Relaxed)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    service.shutdown();

    let frames = tally.frames.load(Ordering::Relaxed);
    let steady = tally.steady_frames.load(Ordering::Relaxed);
    let steady_allocations = tally.steady_allocations.load(Ordering::Relaxed);
    let windows = tally.windows_with_allocations.load(Ordering::Relaxed);
    let worst = tally.worst_window.load(Ordering::Relaxed);
    let copies: usize = styx_frame::copy_counters().iter().map(|c| c.copies()).sum();
    eprintln!(
        "proof: frames={frames} steady_frames={steady} steady_allocations={steady_allocations} \
         windows_with_allocations={windows} worst_window={worst} copies={copies}"
    );
    assert_eq!(copies, 0, "frames were copied");
    assert_eq!(
        steady_allocations, 0,
        "the frame path allocated {steady_allocations} times in {steady} steady frames"
    );
}
