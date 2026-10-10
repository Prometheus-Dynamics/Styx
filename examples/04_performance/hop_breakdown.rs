//! Where a frame's time goes between the sensor and a consumer, hop by hop (`FrameMeta::hops`,
//! `styx::metrics::HopMetrics`), with the copies and dma-buf syncs per frame, for the three
//! ways a consumer gets frames:
//!
//! ```sh
//! # In the capturing process (NV12 1280x800 at 30 fps from the first camera, 20 s):
//! hop_breakdown inproc 20
//! # Through a frame socket to another process:
//! hop_breakdown socket-serve /tmp/hops.sock 25 &   hop_breakdown socket-fetch /tmp/hops.sock 20
//! # Through a camera service to another process:
//! hop_breakdown service-serve /tmp/hops-svc.sock 25 &   hop_breakdown service-client /tmp/hops-svc.sock 20
//! ```
//!
//! Each consumer reads every frame's pixels (one byte per 4 KiB page, so the dma-buf syncs and
//! the mapping happen as for a real consumer) and prints the hop table: p50/p99/max of the time
//! into each hop from the one before it, sensor to the last hop, the copies and dma-buf syncs
//! per frame in its process (and, for the IPC paths, in the serving process), and the heap
//! allocations per frame of its whole process (every thread: the capture, the ISP and its 3A
//! algorithms included; Rust's heap only, not libcamera's C++ one). Allocations are sampled in
//! one-second windows: the median window is the steady state, the whole run's average includes
//! one-off work such as answering a statistics request or a client joining or leaving.
//!
//! The frame-socket consumer takes each frame once, as it is published
//! (`FrameFetcher::fetch_next`, the socket's `<path>.next` endpoint); the server's counts of
//! sends, distinct frames and repeated sends say whether any frame went out twice.
//!
//! The camera is the first one the enabled backends probe (`--features frame-socket,native`
//! for a native camera; `--features frame-socket,libcamera` probes libcamera only, so a
//! libcamera camera, e.g. through the Raspberry Pi PiSP, is used).
//!
//! `STYX_ALLOC_TRACE=1` names the allocations: the call stacks of the allocations in a window of
//! `STYX_ALLOC_TRACE_FRAMES` frames (default 60) of the steady state, grouped and sorted, printed
//! after the table (`alloc_trace.rs`); `scripts/symbolize-alloc-trace.sh` names the call sites.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use styx::ipc::{CameraService, FrameClient, FrameFetcher, FrameSocket, frame_socket};
use styx::metrics::{HopMetrics, PathMetrics};
use styx::prelude::*;

#[path = "hop_breakdown/alloc_trace.rs"]
mod alloc_trace;

/// Counts the process's heap allocations.
struct Counting;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards to the system allocator unchanged; counting is one relaxed atomic add.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        alloc_trace::record();
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        alloc_trace::record();
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        alloc_trace::record();
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

fn allocations() -> u64 {
    ALLOCATIONS.load(Ordering::Relaxed)
}

/// Heap allocations per frame, sampled in one-second windows.
struct AllocRate {
    start: (u64, u64),
    last: (u64, u64),
    at: Instant,
    windows: Vec<f64>,
}

impl AllocRate {
    /// Starts counting now, `frames` frames seen so far.
    fn start(frames: u64) -> Self {
        alloc_trace::arm(frames);
        let now = (allocations(), frames);
        Self {
            start: now,
            last: now,
            at: Instant::now(),
            windows: Vec::with_capacity(64),
        }
    }

    /// `frames` seen so far: closes a window when a second has passed.
    fn sample(&mut self, frames: u64) {
        alloc_trace::frames(frames);
        if self.at.elapsed() < Duration::from_secs(1) {
            return;
        }
        let now = (allocations(), frames);
        let n = now.1.saturating_sub(self.last.1);
        if n > 0 {
            self.windows
                .push(now.0.saturating_sub(self.last.0) as f64 / n as f64);
        }
        self.last = now;
        self.at = Instant::now();
    }

    fn print(&mut self, title: &str, frames: u64) {
        let whole = allocations().saturating_sub(self.start.0) as f64
            / frames.saturating_sub(self.start.1).max(1) as f64;
        self.windows.sort_by(f64::total_cmp);
        let median = self.windows.get(self.windows.len() / 2).copied();
        println!(
            "{title}: {} heap allocations per frame in steady state (median of {} one-second windows), {whole:.2} over the whole run (whole process, every thread)",
            median.map_or_else(|| "-".into(), |m| format!("{m:.2}")),
            self.windows.len(),
        );
        alloc_trace::report(frames);
    }
}

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;
const FPS: u32 = 30;

fn request() -> FrameRequest {
    Frames::nv12().size(WIDTH, HEIGHT).fps(FPS)
}

/// Touches one byte per page of every plane, as a consumer reading the frame would.
fn touch(frame: &FrameLease) -> u8 {
    frame
        .planes()
        .iter()
        .flat_map(|p| p.data().iter().step_by(4096))
        .fold(0u8, |a, &b| a ^ b)
}

fn ms(v: Option<f64>) -> String {
    v.map_or_else(|| "-".into(), |v| format!("{v:.3}"))
}

fn table(title: &str, hops: &HopMetrics) {
    println!(
        "{title}: {} frames ({} zero-copy, {} copied, {} B copied)",
        hops.frames, hops.zero_copy, hops.copied, hops.copied_bytes
    );
    println!(
        "  {:<22} {:>9} {:>9} {:>9}",
        "hop (ms)", "p50", "p99", "max"
    );
    for w in &hops.hops {
        println!(
            "  {:<22} {:>9} {:>9} {:>9}",
            format!("{} -> {}", w.from, w.to),
            ms(w.window.p50_ms),
            ms(w.window.p99_ms),
            ms(w.window.max_ms)
        );
    }
    println!(
        "  {:<22} {:>9} {:>9} {:>9}",
        "total",
        ms(hops.total.p50_ms),
        ms(hops.total.p99_ms),
        ms(hops.total.max_ms)
    );
}

/// Copies and syncs per frame between two readings of a process's path counters.
fn per_frame(title: &str, before: &PathMetrics, after: &PathMetrics, frames: u64) {
    let frames = frames.max(1) as f64;
    let (c0, b0) = before.total_copies();
    let (c1, b1) = after.total_copies();
    let syncs = after.dmabuf_syncs.saturating_sub(before.dmabuf_syncs) as f64;
    let sync_us = after.dmabuf_sync_ns.saturating_sub(before.dmabuf_sync_ns) as f64 / 1e3;
    println!(
        "{title}: {:.2} copies/frame ({:.0} B/frame), {:.2} dma-buf syncs/frame ({:.1} us/frame), {} pools exhausted",
        (c1 - c0) as f64 / frames,
        (b1 - b0) as f64 / frames,
        syncs / frames,
        sync_us / frames,
        after.pool_exhausted.saturating_sub(before.pool_exhausted)
    );
    for (a, b) in before.copies.iter().zip(&after.copies) {
        if b.copies > a.copies {
            println!(
                "    {}: {} copies, {} B",
                b.site,
                b.copies - a.copies,
                b.bytes - a.bytes
            );
        }
    }
}

fn camera() -> Result<ProbedDevice, Box<dyn std::error::Error>> {
    Ok(probe_all().into_iter().next().ok_or("no camera")?)
}

fn inproc(seconds: u64) -> Result<(), Box<dyn std::error::Error>> {
    let mut frames = request().open(&camera()?)?;
    print!("{}", frames.plan());
    // Warm up, then measure.
    let warm = Instant::now() + Duration::from_secs(2);
    while Instant::now() < warm {
        let _ = frames.next_frame(Duration::from_millis(500));
    }
    let before = styx::metrics::path();
    let mut allocs = AllocRate::start(0);
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut n = 0u64;
    let mut x = 0u8;
    while Instant::now() < end {
        if let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_millis(500)) {
            x ^= touch(&frame);
            n += 1;
        }
        allocs.sample(n);
    }
    allocs.print("process", n);
    let after = styx::metrics::path();
    let m = frames.capture().camera_metrics();
    table(
        &format!("in-process ({} {}, {n} frames read, x={x})", m.name, m.mode),
        &m.path,
    );
    per_frame("process", &before, &after, n);
    frames.stop();
    Ok(())
}

fn socket_serve(path: &str, seconds: u64) -> Result<(), Box<dyn std::error::Error>> {
    let mut frames = request().plan_best(&probe_all())?.exportable().start()?;
    let socket = FrameSocket::bind(path)?;
    println!("serving {path} for {seconds} s");
    let start = Instant::now();
    let end = start + Duration::from_secs(seconds);
    let (mut allocs, mut n) = (None::<AllocRate>, 0u64);
    while Instant::now() < end {
        if let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_millis(500)) {
            socket.publish(&frame)?;
            n += 1;
        }
        if start.elapsed() > Duration::from_secs(3) {
            allocs.get_or_insert_with(|| AllocRate::start(n)).sample(n);
        }
    }
    if let Some(a) = &mut allocs {
        a.print("server process, after 3 s", n);
    }
    let m = socket.metrics();
    table("frame socket server (sensor to send)", &m.hops);
    println!(
        "sends: {} ({} distinct frames, {} repeated) of {} published; leases hold ms p50/p99/max {}/{}/{}",
        m.served,
        m.served_frames,
        m.repeated,
        m.published,
        ms(m.hold.p50_ms),
        ms(m.hold.p99_ms),
        ms(m.hold.max_ms)
    );
    frames.stop();
    Ok(())
}

fn socket_fetch(path: &str, seconds: u64) -> Result<(), Box<dyn std::error::Error>> {
    // Each frame once, as it is published: no polling, no frame sent twice.
    let mut fetcher = FrameFetcher::new(path);
    let warm = Instant::now() + Duration::from_secs(2);
    while Instant::now() < warm {
        drop(fetcher.fetch_next(Duration::from_secs(2))?);
    }
    let server_before = frame_socket::fetch_metrics(path)?;
    let mut fetcher = FrameFetcher::new(path);
    let before = styx::metrics::path();
    let mut allocs = AllocRate::start(0);
    let end = Instant::now() + Duration::from_secs(seconds);
    let (mut n, mut x) = (0u64, 0u8);
    while Instant::now() < end {
        let frame = fetcher.fetch_next(Duration::from_secs(2))?;
        x ^= touch(&frame);
        n += 1;
        drop(frame);
        allocs.sample(n);
    }
    allocs.print("consumer process", n);
    let after = styx::metrics::path();
    let server = frame_socket::fetch_metrics(path)?;
    let f = fetcher.stats();
    table(
        &format!(
            "frame socket consumer ({n} frames read, {} repeated, {} polled, x={x})",
            f.repeated, f.polled
        ),
        &fetcher.hop_metrics(),
    );
    per_frame("consumer process", &before, &after, n);
    per_frame(
        "server process",
        &server_before.snapshot.process.path,
        &server.snapshot.process.path,
        n,
    );
    println!(
        "server, meanwhile: {} sends, {} distinct frames, {} repeated sends, {} published",
        server.served - server_before.served,
        server.served_frames - server_before.served_frames,
        server.repeated - server_before.repeated,
        server.published - server_before.published,
    );
    println!(
        "leases: hold ms p50/p99/max {}/{}/{}",
        ms(server.hold.p50_ms),
        ms(server.hold.p99_ms),
        ms(server.hold.max_ms)
    );
    Ok(())
}

fn service_serve(path: &str, seconds: u64) -> Result<(), Box<dyn std::error::Error>> {
    let service = CameraService::new(camera()?).keep_streaming().serve(path)?;
    println!("camera service on {path} for {seconds} s");
    // Clients connect and warm up meanwhile; allocations are counted from then on.
    std::thread::sleep(Duration::from_secs(5.min(seconds)));
    let mut allocs = AllocRate::start(service.stats().sent);
    let end = Instant::now() + Duration::from_secs(seconds.saturating_sub(5));
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(100));
        allocs.sample(service.stats().sent);
    }
    allocs.print("service process, after 5 s", service.stats().sent);
    let m = service.metrics();
    for c in &m.client_metrics {
        if let Some(h) = &c.hops {
            table(&format!("service, {}", c.label), h);
        }
    }
    Ok(())
}

fn service_client(path: &str, seconds: u64) -> Result<(), Box<dyn std::error::Error>> {
    let client = FrameClient::request(path, &Frames::nv12().size(WIDTH, HEIGHT).fps(FPS))?;
    print!("{}", client.plan().unwrap_or_default());
    let warm = Instant::now() + Duration::from_secs(2);
    while Instant::now() < warm {
        let _ = client.recv(Duration::from_millis(500));
    }
    let server_before = FrameClient::service_metrics(path)?.snapshot.process.path;
    let before = styx::metrics::path();
    let mut allocs = AllocRate::start(0);
    let end = Instant::now() + Duration::from_secs(seconds);
    let (mut n, mut x) = (0u64, 0u8);
    while Instant::now() < end {
        if let RecvOutcome::Data(frame) = client.recv(Duration::from_millis(500)) {
            x ^= touch(&frame);
            n += 1;
        }
        allocs.sample(n);
    }
    allocs.print("client process", n);
    let after = styx::metrics::path();
    let service = FrameClient::service_metrics(path)?;
    table(
        &format!("camera service client ({n} frames read, x={x})"),
        &client.hop_metrics(),
    );
    per_frame("client process", &before, &after, n);
    per_frame(
        "service process",
        &server_before,
        &service.snapshot.process.path,
        n,
    );
    for c in &service.client_metrics {
        println!(
            "{}: hold ms p50/p99/max {}/{}/{}",
            c.label,
            ms(c.hold.p50_ms),
            ms(c.hold.p99_ms),
            ms(c.hold.max_ms)
        );
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    alloc_trace::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    let secs = |i: usize| arg(i).and_then(|s| s.parse().ok()).unwrap_or(20);
    match arg(0) {
        Some("inproc") => inproc(secs(1)),
        Some("socket-serve") => socket_serve(arg(1).ok_or("socket path")?, secs(2)),
        Some("socket-fetch") => socket_fetch(arg(1).ok_or("socket path")?, secs(2)),
        Some("service-serve") => service_serve(arg(1).ok_or("socket path")?, secs(2)),
        Some("service-client") => service_client(arg(1).ok_or("socket path")?, secs(2)),
        _ => Err("usage: hop_breakdown inproc [S] | socket-serve PATH [S] | socket-fetch PATH [S] | service-serve PATH [S] | service-client PATH [S]".into()),
    }
}
