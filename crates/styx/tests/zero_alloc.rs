//! The steady-state frame path to a consumer in another process is zero-copy and allocates
//! nothing per frame beyond each received frame's one release record, measured with a counting
//! allocator over many frames after a warm-up, thread by thread:
//!
//! - a camera service: the virtual camera's capture thread, the service's thread for the client
//!   (taking the frame, exporting, sending, taking releases), the client receiving, importing,
//!   reading and releasing each frame (test thread).
//!
//! - a camera service's client without a thread: `poll(2)` and `try_next`, and `next().await`
//!   on styx-graph's `block_on` (its reactor thread allocating nothing per frame either).
//!
//! - with `daedalus`: a camera service's client reading each frame through Daedalus's generic
//!   frame view (`FrameView`, `daedalus:frame` v2), borrowed from the frame and from a payload
//!   retyped by Styx's provider, as a node in a separately built plugin reads it.
//!
//! And the copy counters (`HopMetrics`, `styx::metrics::path`) say nothing was copied.
//!
//! The counts are exact, not bounds: each frame's own records and nothing else, whatever the
//! timing (the proof runs under CPU load too). Two things allocate once rather than per frame,
//! and only when timing makes them happen, so they are kept out of the windows: parking_lot's
//! table of waiting threads, made before any window ([`presize_parking_table`]), and a
//! consumer's mapping of a buffer: a pool's buffers are used in turn, so each stays mapped once
//! mapped in the warm-up, and only a buffer that a pool which ran out made meanwhile is mapped
//! in a window, one allocation each, counted as such.
//!
//! `STYX_ZERO_ALLOC_TRACE=1` prints the call stack of every counted allocation (the rare ones
//! symbolised) as the test ends, to name an unexpected one.

#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;

/// Counts allocations while armed, by the kind of thread making them.
struct Counting;

static ARMED: AtomicBool = AtomicBool::new(false);
/// Allocations by thread kind: [`Kind`].
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum Kind {
    /// The test thread: the client.
    Test = 0,
    /// A camera service's thread for one client.
    ServiceClient = 1,
    /// Anything else (capture workers).
    Other = 2,
    /// styx-graph's reactor thread (waking awaited frame clients).
    Reactor = 3,
}

thread_local! {
    static KIND: Cell<Option<Kind>> = const { Cell::new(None) };
    static IS_TEST: Cell<bool> = const { Cell::new(false) };
}

/// This thread's kind, from its name (read without allocating).
fn kind() -> Kind {
    if IS_TEST.try_with(Cell::get).unwrap_or(false) {
        return Kind::Test;
    }
    if let Ok(Some(k)) = KIND.try_with(Cell::get) {
        return k;
    }
    let mut name = [0u8; 32];
    // SAFETY: writes at most `name.len()` bytes (NUL-terminated) for the calling thread.
    unsafe { libc::pthread_getname_np(libc::pthread_self(), name.as_mut_ptr().cast(), name.len()) };
    let k = if name.starts_with(b"styx-camera-cli") {
        Kind::ServiceClient
    } else if name.starts_with(b"styx-reactor") {
        Kind::Reactor
    } else {
        Kind::Other
    };
    let _ = KIND.try_with(|c| c.set(Some(k)));
    k
}

fn note() {
    if ARMED.load(Ordering::Relaxed) && !trace::inside() {
        let k = kind();
        COUNTS[k as usize].fetch_add(1, Ordering::Relaxed);
        trace::record(k);
    }
}

// SAFETY: every method forwards to `System` unchanged; the bookkeeping only touches atomics and
// const-initialised thread-locals, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// What a measurement window counted.
struct Counted {
    /// Allocations by thread kind ([`Kind`]).
    kinds: [u64; 4],
    /// Buffers a consumer in this process mapped (`frame_maps`): each is one allocation, the
    /// mapping its frames share (`styx::ipc`'s map cache).
    maps: u64,
    /// Times a buffer pool in this process found every buffer held and made another one, a
    /// buffer its consumers had not mapped yet.
    pool_grew: u64,
}

impl std::ops::Index<Kind> for Counted {
    type Output = u64;

    fn index(&self, kind: Kind) -> &u64 {
        &self.kinds[kind as usize]
    }
}

impl Counted {
    /// The consumer mapped only the buffers a pool made in the window (none, unless frames
    /// were held long enough for the capture to run out of buffers): a buffer it had mapped
    /// before was not dropped from its cache and mapped again.
    fn assert_maps_only_new_buffers(&self) {
        assert!(
            self.maps <= self.pool_grew,
            "{} buffers mapped (again) with {} new ones",
            self.maps,
            self.pool_grew
        );
    }
}

/// What the window counted while `run` runs (armed for its duration). With
/// `STYX_ZERO_ALLOC_TRACE=1`, every allocation's call stack, printed as the test ends.
#[track_caller]
fn counted(run: impl FnOnce()) -> Counted {
    let at = std::panic::Location::caller();
    trace::init();
    for c in &COUNTS {
        c.store(0, Ordering::Relaxed);
    }
    let path = styx::core::metrics::path_counters();
    let (maps, pool_grew) = (path.frame_maps.get(), path.pool_exhausted.get());
    ARMED.store(true, Ordering::SeqCst);
    run();
    ARMED.store(false, Ordering::SeqCst);
    trace::report(&at.to_string());
    Counted {
        kinds: std::array::from_fn(|i| COUNTS[i].load(Ordering::Relaxed)),
        maps: path.frame_maps.get() - maps,
        pool_grew: path.pool_exhausted.get() - pool_grew,
    }
}

/// parking_lot parks a waiting thread in one process-wide table, made on the first lock any
/// thread had to wait for and grown (to three slots per thread) when a thread first waits on
/// one with more threads alive than it has room for: an allocation or two, once, on whichever
/// thread first waits, which in the frame paths is a matter of timing (a lock taken while
/// another thread holds it). Done here first, for more threads than the test ever has, so it
/// is never counted.
fn presize_parking_table() {
    const THREADS: usize = 64;
    let shared = Arc::new((Mutex::new((0usize, false)), parking_lot::Condvar::new()));
    let threads: Vec<_> = (0..THREADS)
        .map(|_| {
            let shared = shared.clone();
            std::thread::spawn(move || {
                let (lock, cv) = &*shared;
                let mut state = lock.lock();
                state.0 += 1;
                cv.notify_all();
                // Waiting gives the thread its parking data (growing the table) before it
                // lets go of the lock.
                while !state.1 {
                    cv.wait(&mut state);
                }
            })
        })
        .collect();
    {
        let (lock, cv) = &*shared;
        let mut state = lock.lock();
        // Seen with the lock: every thread is waiting (its parking data made).
        while state.0 < THREADS {
            cv.wait(&mut state);
        }
        state.1 = true;
        cv.notify_all();
    }
    for t in threads {
        t.join().unwrap();
    }
}

/// Allocations on the test thread while `run` runs.
fn counted_here<R>(totals: &mut u64, run: impl FnOnce() -> R) -> R {
    let before = COUNTS[Kind::Test as usize].load(Ordering::Relaxed);
    let r = run();
    *totals += COUNTS[Kind::Test as usize].load(Ordering::Relaxed) - before;
    r
}

const FRAMES: u64 = 300;
const WIDTH: u32 = 320;
const HEIGHT: u32 = 200;

fn read(frame: &FrameLease) -> u8 {
    frame
        .planes()
        .iter()
        .flat_map(|p| p.data().iter().step_by(1024))
        .fold(0, |a, &b| a ^ b)
}

fn socket_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "styx-zero-alloc-{name}-{}.sock",
        std::process::id()
    ))
}

fn camera_service() {
    let res = Resolution::new(WIDTH, HEIGHT).unwrap();
    let mode = Mode::with_interval(
        MediaFormat::new(FourCc::NV12, res, ColorSpace::Srgb),
        Interval::from_fps(200).unwrap(),
    );
    let device = styx::capture_api::make_virtual_device("zero-alloc", [mode]);
    let path = socket_path("service");
    let service = CameraService::new(device)
        .keep_streaming()
        .serve(&path)
        .unwrap();
    let client = FrameClient::request(&path, &Frames::nv12().size(WIDTH, HEIGHT)).unwrap();
    let next = |client: &FrameClient| loop {
        if let RecvOutcome::Data(frame) = client.recv(Duration::from_secs(2)) {
            return frame;
        }
    };
    IS_TEST.with(|t| t.set(true));
    for _ in 0..100 {
        std::hint::black_box(read(&next(&client)));
    }
    let copies_before = styx::metrics::path().total_copies();
    let mut receive = 0;
    let counts = counted(|| {
        for _ in 0..FRAMES {
            let frame = counted_here(&mut receive, || next(&client));
            std::hint::black_box(read(&frame));
        }
    });
    let per = |n: u64| n as f64 / FRAMES as f64;
    println!(
        "camera service, allocations per frame: capture (virtual camera) {:.2}, service thread for the client {:.2}, client receive + import + read + release {:.2}",
        per(counts[Kind::Other]),
        per(counts[Kind::ServiceClient]),
        per(counts[Kind::Test]),
    );
    assert_eq!(
        counts[Kind::ServiceClient],
        0,
        "the service's thread allocates per frame"
    );
    // One refcounted record per received frame (its release), and the mapping of a buffer
    // the capture's pool made meanwhile, if it had to.
    counts.assert_maps_only_new_buffers();
    assert_eq!(
        counts[Kind::Test],
        FRAMES + counts.maps,
        "the client allocates {} times for {FRAMES} frames ({} buffers mapped)",
        counts[Kind::Test],
        counts.maps
    );
    assert_eq!(
        styx::metrics::path().total_copies(),
        copies_before,
        "frames were copied"
    );
    let hops = client.hop_metrics();
    assert_eq!(hops.copied, 0);
    for hop in ["queued", "taken", "sent", "received", "imported"] {
        assert!(hops.hop(hop).is_some(), "no {hop} hop: {hops:?}");
    }
    IS_TEST.with(|t| t.set(false));
    // The client's receive and import times came back with its releases: looked at while it is
    // connected (the service forgets a client's statistics as it leaves).
    let t = std::time::Instant::now();
    loop {
        let m = service.metrics();
        let hops = m.client_metrics.first().and_then(|c| c.hops.clone());
        if let Some(h) = hops.filter(|h| h.hop("imported").is_some() && h.frames > FRAMES) {
            assert!(
                h.hop("sent").is_some() && h.hop("received").is_some(),
                "{h:?}"
            );
            assert_eq!(h.copied, 0);
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "no client hops: {:?}",
            m.client_metrics
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(client);
    drop(service);
}

/// A camera service's client without a thread of its own: `poll(2)` on its descriptor and
/// `try_next`, then frames awaited with `next()` (one `block_on` over all of them, so the
/// executor's own waker is made once). Each allocates nothing per frame beyond the frame's
/// release record, and the reactor's thread (styx-graph) nothing at all.
fn camera_service_without_a_thread() {
    let res = Resolution::new(WIDTH, HEIGHT).unwrap();
    let mode = Mode::with_interval(
        MediaFormat::new(FourCc::NV12, res, ColorSpace::Srgb),
        Interval::from_fps(200).unwrap(),
    );
    let device = styx::capture_api::make_virtual_device("zero-alloc-poll", [mode]);
    let path = socket_path("service-poll");
    let _service = CameraService::new(device)
        .keep_streaming()
        .serve(&path)
        .unwrap();
    let client = FrameClient::request(&path, &Frames::nv12().size(WIDTH, HEIGHT)).unwrap();
    let polled = |client: &FrameClient| loop {
        let mut fd = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(client),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        unsafe { libc::poll(&mut fd, 1, 2000) };
        if let RecvOutcome::Data(frame) = client.try_next() {
            return frame;
        }
    };
    let awaited = |client: &FrameClient, frames: u64| {
        styx_graph::rt::block_on(async {
            for _ in 0..frames {
                match client.next().await {
                    RecvOutcome::Data(frame) => {
                        std::hint::black_box(read(&frame));
                    }
                    _ => panic!("closed"),
                }
            }
        });
    };
    IS_TEST.with(|t| t.set(true));
    for _ in 0..100 {
        std::hint::black_box(read(&polled(&client)));
    }
    awaited(&client, 100);
    let counts = counted(|| {
        for _ in 0..FRAMES {
            std::hint::black_box(read(&polled(&client)));
        }
    });
    let per = |n: u64| n as f64 / FRAMES as f64;
    println!(
        "camera service, poll + try_next: client {:.2} allocations per frame, service thread {:.2}",
        per(counts[Kind::Test]),
        per(counts[Kind::ServiceClient]),
    );
    counts.assert_maps_only_new_buffers();
    assert_eq!(
        counts[Kind::Test],
        FRAMES + counts.maps,
        "poll + try_next allocates {} times for {FRAMES} frames ({} buffers mapped)",
        counts[Kind::Test],
        counts.maps
    );
    assert_eq!(counts[Kind::ServiceClient], 0);
    let counts = counted(|| awaited(&client, FRAMES));
    println!(
        "camera service, next().await: client {:.2} allocations per frame, reactor thread {:.2}",
        per(counts[Kind::Test]),
        per(counts[Kind::Reactor]),
    );
    assert_eq!(counts[Kind::Reactor], 0, "the reactor allocates");
    // The frames' release records (block_on reuses its thread's waker).
    counts.assert_maps_only_new_buffers();
    assert_eq!(
        counts[Kind::Test],
        FRAMES + counts.maps,
        "next().await allocates {} times for {FRAMES} frames ({} buffers mapped)",
        counts[Kind::Test],
        counts.maps
    );
    assert_eq!(client.hop_metrics().copied, 0);
    IS_TEST.with(|t| t.set(false));
}

#[path = "zero_alloc/trace.rs"]
mod trace;

/// A stage's timing window (`StageMetrics`, recorded per frame, e.g. by `recv`) is made whole
/// on its first sample and never grows past it (it grew by doubling to 128 samples: an
/// allocation now and then in the first seconds).
fn stage_window() {
    let stage = styx::metrics::StageMetrics::default();
    stage.record(Duration::from_millis(1));
    let c = counted(|| {
        for i in 0..1000 {
            stage.record(Duration::from_micros(i));
        }
    });
    assert_eq!(c.kinds.iter().sum::<u64>(), 0, "the stage window allocates");
    assert_eq!(stage.samples(), 120);
}

#[cfg(feature = "daedalus")]
#[path = "zero_alloc/daedalus.rs"]
mod daedalus;

/// One test: the counting is process-wide, so nothing else may run meanwhile.
#[test]
fn steady_state_ipc_paths_do_not_allocate_or_copy() {
    let _flush = trace::Flush;
    presize_parking_table();
    stage_window();
    camera_service();
    camera_service_without_a_thread();
    #[cfg(feature = "daedalus")]
    daedalus::camera_service_frame_view();
}
