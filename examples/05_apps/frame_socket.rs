//! The latest frame for other processes, leased: a camera process publishes every frame on a
//! `FrameSocket`; any process fetches the latest one (`fetch_frame`) as dma-bufs (no copy) and
//! holds it as long as it needs: the connection is the lease, and the camera buffer behind it
//! is not written again until the consumer drops the frame. The camera never waits: with every
//! buffer held, it drops frames until one comes back. (For consumers that ask for their own
//! size and format, see `camera_service`.)
//!
//! ```sh
//! cargo run -p styx-examples --features native,v4l2 --bin frame_socket -- serve /tmp/cam.sock 20
//! cargo run -p styx-examples --features native,v4l2 --bin frame_socket -- fetch /tmp/cam.sock 10 300
//! ```
//!
//! `fetch SOCKET COUNT HOLD_MS` takes COUNT frames, holds each for HOLD_MS and checks that its
//! pixels did not change meanwhile. The wire format is HeliOS's `styx-frame-lease-v1`.

use std::time::{Duration, Instant};

use styx::ipc::{FrameSocket, fetch_frame};
use styx::prelude::*;

fn checksum(frame: &FrameLease) -> u64 {
    frame
        .planes()
        .iter()
        .flat_map(|p| p.data().iter().step_by(97))
        .fold(0u64, |h, &b| h.wrapping_mul(31).wrapping_add(u64::from(b)))
}

fn serve(path: &str, seconds: u64) -> Result<(), Box<dyn std::error::Error>> {
    let wants = Frames::nv12().fps(30);
    // `exportable`: frames the plan converts go straight into shareable memory (memfds), so
    // publishing them copies nothing; camera buffers (dma-bufs) are shared as they are.
    let plan = wants.plan_best(&styx::probe_all())?.exportable();
    print!("{plan}");
    let mut frames = plan.start()?;
    let socket = FrameSocket::bind(path)?;
    println!("serving the latest frame on {path} for {seconds} s");
    let started = Instant::now();
    let mut shown = Instant::now();
    while started.elapsed() < Duration::from_secs(seconds) {
        if let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_millis(500)) {
            socket.publish(&frame)?;
        }
        if shown.elapsed() > Duration::from_secs(5) {
            shown = Instant::now();
            println!("{:?}", socket.stats());
        }
    }
    println!("{:?}", socket.stats());
    frames.stop();
    Ok(())
}

fn fetch(path: &str, count: usize, hold: Duration) -> Result<(), Box<dyn std::error::Error>> {
    let mut last = None;
    for _ in 0..count {
        let t = Instant::now();
        let frame = fetch_frame(path, Duration::from_secs(2))?;
        let fetched = t.elapsed();
        let meta = frame.meta();
        // Native and UVC cameras stamp frames with CLOCK_MONOTONIC, the same in every process
        // (other sources, such as v4l2loopback, have their own clock: no age then).
        let now = TimestampClock::Monotonic.now_ns().unwrap_or(0);
        let age = now.saturating_sub(meta.timestamp) as f64 / 1e6;
        let age = if age < 10_000.0 {
            format!(", {age:.1} ms old")
        } else {
            String::new()
        };
        let before = checksum(&frame);
        std::thread::sleep(hold); // work on the frame; the server keeps its buffer
        let unchanged = checksum(&frame) == before;
        println!(
            "{} {}x{} t={:.3} s via {:?}, {} planes, fetched in {:.2} ms{age}, \
             unchanged after {} ms: {unchanged}{}",
            meta.format.code,
            meta.format.resolution.width,
            meta.format.resolution.height,
            meta.timestamp as f64 / 1e9,
            meta.residency,
            frame.planes().len(),
            fetched.as_secs_f64() * 1e3,
            hold.as_millis(),
            if last == Some(meta.timestamp) {
                " (same frame as before)"
            } else {
                ""
            }
        );
        last = Some(meta.timestamp);
        // Dropping the frame closes the connection: the lease ends.
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.get(1).map_or("/tmp/styx-frame.sock", String::as_str);
    let num = |i: usize, default: u64| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(default);
    match args.first().map(String::as_str) {
        Some("serve") => serve(path, num(2, 20)),
        Some("fetch") => fetch(
            path,
            num(2, 10) as usize,
            Duration::from_millis(num(3, 300)),
        ),
        _ => Err(
            "usage: frame_socket serve SOCKET [seconds] | fetch SOCKET [count] [hold-ms]".into(),
        ),
    }
}
