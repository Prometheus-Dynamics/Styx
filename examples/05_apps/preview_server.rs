//! A camera preview in the browser next to a vision consumer, from a camera service, over a
//! tiny HTTP server (std only, no framework), with measurements every second.
//!
//! - A camera service serves a virtual 1280x800 camera at 30 fps (or `--camera NAME` from the
//!   service at `--socket PATH`, e.g. a real one).
//! - A "vision" client takes every frame at full size, as a detector would.
//! - A preview (`styx::preview`, a low-priority client) makes 640x400 JPEGs at up to 15 fps.
//! - `http://127.0.0.1:8090/` shows it: `/stream.mjpg` (MJPEG), `/frame.jpg` (latest JPEG),
//!   `/frame.bin` (the WebSocket message format), `/metrics` (Prometheus text).
//!
//! ```text
//! cargo run --release -p styx-examples --features preview --bin preview_server
//! cargo run --release -p styx-examples --features preview --bin preview_server -- \
//!     --seconds 20 --size 320x200 --fps 10 --quality 60 --port 8090
//! ```
//!
//! Each second: the vision client's rate and latency, the service's restarts, and the
//! preview's frames, drops, encode time, bytes per frame and CPU.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use styx::capture_api::make_virtual_rgb_device;
use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;
use styx::preview::{MJPEG_CONTENT_TYPE, MjpegStream, Preview, PreviewConfig, ws_message};

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// Wakes a thread parked on a stream.
struct Unpark(std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Serve the MJPEG body on a blocking socket: poll the stream, park until woken.
fn stream_mjpeg(mut socket: TcpStream, mut body: MjpegStream) -> std::io::Result<()> {
    write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Type: {MJPEG_CONTENT_TYPE}\r\nCache-Control: no-cache\r\n\
         Connection: close\r\n\r\n"
    )?;
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        match body.poll_chunk(&mut cx) {
            Poll::Ready(Some(chunk)) => socket.write_all(&chunk)?,
            Poll::Ready(None) => return Ok(()),
            Poll::Pending => std::thread::park_timeout(Duration::from_secs(1)),
        }
    }
}

fn respond(mut socket: TcpStream, status: &str, kind: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        socket,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    socket.write_all(body)
}

const PAGE: &str = "<!doctype html><title>Styx preview</title>\
<body style=\"background:#111;color:#ddd;font-family:sans-serif\">\
<p>Styx preview (MJPEG, latest frame only)</p><img src=\"/stream.mjpg\"></body>";

fn serve(socket: TcpStream, preview: &Preview) -> std::io::Result<()> {
    let mut line = String::new();
    BufReader::new(&socket).read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("/");
    match path {
        "/" => respond(socket, "200 OK", "text/html", PAGE.as_bytes()),
        "/stream.mjpg" => stream_mjpeg(socket, preview.mjpeg()),
        "/frame.jpg" | "/frame.bin" => {
            // One frame: subscribe, so the preview encodes while we wait.
            let mut viewer = preview.subscribe();
            match viewer.recv(Duration::from_secs(5)) {
                Some(frame) if path == "/frame.jpg" => {
                    respond(socket, "200 OK", "image/jpeg", &frame.jpeg)
                }
                Some(frame) => respond(
                    socket,
                    "200 OK",
                    "application/octet-stream",
                    &ws_message(&frame),
                ),
                None => respond(socket, "503 Service Unavailable", "text/plain", b"no frame"),
            }
        }
        "/metrics" => respond(
            socket,
            "200 OK",
            "text/plain; version=0.0.4",
            styx::metrics::snapshot().prometheus_text().as_bytes(),
        ),
        _ => respond(socket, "404 Not Found", "text/plain", b"not found"),
    }
}

/// A virtual 1280x800 camera at 30 fps (RGB: the virtual source fills 3-byte pixels).
fn camera() -> ProbedDevice {
    make_virtual_rgb_device("virtual-1280x800", 1280, 800, 30)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds: Option<u64> = arg("--seconds").and_then(|v| v.parse().ok());
    let port = arg("--port").unwrap_or_else(|| "8090".into());
    let (w, h) = arg("--size")
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((640, 400));
    let fps: f32 = arg("--fps").and_then(|v| v.parse().ok()).unwrap_or(15.0);
    let quality: u8 = arg("--quality").and_then(|v| v.parse().ok()).unwrap_or(70);

    // The camera service: ours with a virtual camera, or one already running.
    let (service, socket) = match arg("--socket") {
        Some(path) => (None, PathBuf::from(path)),
        None => {
            let path =
                std::env::temp_dir().join(format!("styx-preview-{}.sock", std::process::id()));
            let service = CameraService::new(camera()).keep_streaming().serve(&path)?;
            (Some(service), path)
        }
    };
    let options = match arg("--camera") {
        Some(name) => FrameClient::options(&socket).camera(name),
        None => FrameClient::options(&socket),
    };

    // The vision consumer: every frame, full size (NV12 from a real camera; the virtual one
    // gives RGB).
    let vision = match arg("--socket") {
        Some(_) => options.clone().request(&Frames::nv12())?,
        None => options.clone().request(&Frames::rgb())?,
    };
    println!("vision plan:\n{}", vision.plan().unwrap_or_default());
    let (received, latency_us) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let stop = Arc::new(AtomicBool::new(false));
    let vision_thread = {
        let (received, latency_us, stop) = (received.clone(), latency_us.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if let RecvOutcome::Data(frame) = vision.recv(Duration::from_millis(100)) {
                    received.fetch_add(1, Ordering::Relaxed);
                    // Capture to now, for cameras on a system clock (not the virtual one).
                    let meta = frame.meta();
                    if let Some(now) = meta.clock.and_then(TimestampClock::now_ns) {
                        let age = now.saturating_sub(meta.timestamp);
                        latency_us.store(age / 1000, Ordering::Relaxed);
                    }
                }
            }
        })
    };

    let preview = Arc::new(Preview::from_service(
        options,
        PreviewConfig::new()
            .name("camera")
            .size(w, h)
            .max_fps(fps)
            .quality(quality),
    )?);
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))?;
    listener.set_nonblocking(true)?;
    println!("preview on http://{}/", listener.local_addr()?);
    // With nobody at the page, keep one viewer so the measurements below have frames.
    let _measuring = preview.subscribe();

    let started = Instant::now();
    let mut last = (Instant::now(), 0u64);
    while seconds.is_none_or(|s| started.elapsed() < Duration::from_secs(s)) {
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_nonblocking(false)?;
                let preview = preview.clone();
                std::thread::spawn(move || {
                    let _ = serve(socket, &preview);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e.into()),
        }
        if last.0.elapsed() >= Duration::from_secs(1) {
            let now = received.load(Ordering::Relaxed);
            let rate = (now - last.1) as f64 / last.0.elapsed().as_secs_f64();
            last = (Instant::now(), now);
            let m = preview.metrics();
            let restarts = service.as_ref().map_or(0, |s| s.stats().restarts);
            println!(
                "vision {rate:5.1} fps latency {:5.2} ms restarts {restarts} | preview {}x{} \
                 from {}x{} {} frames, dropped rate {} busy {}, encode p50 {:.2} p95 {:.2} ms, \
                 {} B/frame, cpu {:.1}%",
                latency_us.load(Ordering::Relaxed) as f64 / 1000.0,
                m.size.0,
                m.size.1,
                m.source_size.0,
                m.source_size.1,
                m.encoded,
                m.dropped_rate,
                m.dropped_busy,
                m.encode.p50_ms.unwrap_or(0.0),
                m.encode.p95_ms.unwrap_or(0.0),
                m.bytes_per_frame.unwrap_or(0),
                m.cpu_percent.unwrap_or(0.0),
            );
        }
    }
    stop.store(true, Ordering::Release);
    let _ = vision_thread.join();
    Ok(())
}
