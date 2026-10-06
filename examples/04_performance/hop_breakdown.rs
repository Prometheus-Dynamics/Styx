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
//! algorithms included).
//!
//! The camera is the first one the enabled backends probe (`--features frame-socket,native`
//! for a native camera; `--features frame-socket,libcamera` probes libcamera only, so a
//! libcamera camera, e.g. through the Raspberry Pi PiSP, is used).

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use styx::ipc::{CameraService, FrameClient, FrameFetcher, FrameSocket, frame_socket};
use styx::metrics::{HopMetrics, PathMetrics};
use styx::prelude::*;

/// Counts the process's heap allocations.
struct Counting;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards to the system allocator unchanged; counting is one relaxed atomic add.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
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

fn allocations_per_frame(title: &str, since: u64, frames: u64) {
    println!(
        "{title}: {:.2} heap allocations per frame (whole process, every thread)",
        allocations().saturating_sub(since) as f64 / frames.max(1) as f64
    );
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
    let allocs = allocations();
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut n = 0u64;
    let mut x = 0u8;
    while Instant::now() < end {
        if let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_millis(500)) {
            x ^= touch(&frame);
            n += 1;
        }
    }
    allocations_per_frame("process", allocs, n);
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
    let (mut allocs, mut n) = (None, 0u64);
    while Instant::now() < end {
        if let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_millis(500)) {
            socket.publish(&frame)?;
            if start.elapsed() > Duration::from_secs(3) {
                allocs.get_or_insert_with(allocations);
                n += 1;
            }
        }
    }
    allocations_per_frame("server process, after 3 s", allocs.unwrap_or(0), n);
    let m = socket.metrics();
    table("frame socket server (sensor to send)", &m.hops);
    println!(
        "leases: {} served, hold ms p50/p99/max {}/{}/{}",
        m.served,
        ms(m.hold.p50_ms),
        ms(m.hold.p99_ms),
        ms(m.hold.max_ms)
    );
    frames.stop();
    Ok(())
}

fn socket_fetch(path: &str, seconds: u64) -> Result<(), Box<dyn std::error::Error>> {
    let mut fetcher = FrameFetcher::new(path);
    let mut last = 0u64;
    let fetch_new = |fetcher: &mut FrameFetcher, last: &mut u64| -> Option<FrameLease> {
        let frame = fetcher.fetch(Duration::from_secs(2)).ok()?;
        if frame.meta().timestamp == *last {
            return None;
        }
        *last = frame.meta().timestamp;
        Some(frame)
    };
    let warm = Instant::now() + Duration::from_secs(2);
    while Instant::now() < warm {
        if fetch_new(&mut fetcher, &mut last).is_none() {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let server_before = frame_socket::fetch_metrics(path)?.snapshot.process.path;
    let mut fetcher = FrameFetcher::new(path);
    let before = styx::metrics::path();
    let allocs = allocations();
    let end = Instant::now() + Duration::from_secs(seconds);
    let (mut n, mut x) = (0u64, 0u8);
    while Instant::now() < end {
        match fetch_new(&mut fetcher, &mut last) {
            Some(frame) => {
                x ^= touch(&frame);
                n += 1;
            }
            // The same frame again: wait a little for the next.
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    allocations_per_frame("consumer process", allocs, n);
    let after = styx::metrics::path();
    let server = frame_socket::fetch_metrics(path)?;
    table(
        &format!("frame socket consumer ({n} frames read, x={x})"),
        &fetcher.hop_metrics(),
    );
    per_frame("consumer process", &before, &after, n);
    per_frame(
        "server process",
        &server_before,
        &server.snapshot.process.path,
        n,
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
    let (allocs, sent) = (allocations(), service.stats().sent);
    std::thread::sleep(Duration::from_secs(seconds.saturating_sub(5)));
    allocations_per_frame(
        "service process, after 5 s",
        allocs,
        service.stats().sent.saturating_sub(sent),
    );
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
    let allocs = allocations();
    let end = Instant::now() + Duration::from_secs(seconds);
    let (mut n, mut x) = (0u64, 0u8);
    while Instant::now() < end {
        if let RecvOutcome::Data(frame) = client.recv(Duration::from_millis(500)) {
            x ^= touch(&frame);
            n += 1;
        }
    }
    allocations_per_frame("client process", allocs, n);
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
