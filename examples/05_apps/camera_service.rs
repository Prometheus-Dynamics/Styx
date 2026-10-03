//! Cameras for many processes. Run the service once, then any number of clients, each asking a
//! camera for the frames it needs; the service plans one shared capture per camera and passes
//! frames as file descriptors (camera buffers, memfds), without copying.
//!
//! ```text
//! camera_service serve [camera]                    # every camera, or the one named; socket:
//!                                                  #   $STYX_SOCKET or /tmp/styx-camera.sock
//! camera_service cameras                           # what the service serves
//! camera_service client [--camera NAME] luma [WxH] [seconds]   # also: rgb, nv12, mjpg, h264
//! ```

use std::time::{Duration, Instant};

use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;

fn socket_path() -> String {
    std::env::var("STYX_SOCKET").unwrap_or_else(|_| "/tmp/styx-camera.sock".into())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve(args.get(1).map(String::as_str)),
        Some("cameras") => {
            for camera in FrameClient::cameras(socket_path())? {
                let state = if camera.in_use { "in use" } else { "idle" };
                println!("{} ({state}) {}", camera.name, camera.keys.join(" "));
            }
            Ok(())
        }
        Some("client") => client(&args[1..]),
        _ => Err(
            "usage: camera_service serve [camera] | cameras | client [--camera NAME] \
                  <luma|rgb|nv12|mjpg|h264> [WxH] [seconds]"
                .into(),
        ),
    }
}

fn serve(name: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let service = match name {
        Some(name) => CameraService::new(
            probe_all()
                .into_iter()
                .find(|d| d.identity.display.contains(name))
                .ok_or("no such camera")?,
        ),
        None => CameraService::all_cameras(),
    };
    let handle = service.serve(socket_path())?;
    println!("serving on {}", socket_path());
    for camera in handle.cameras() {
        println!("  {}", camera.name);
    }
    let mut shown = None;
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let plan = handle.plan();
        if plan != shown {
            println!("{}", plan.as_deref().unwrap_or("(no capture)\n"));
            shown = plan;
        }
        println!("{:?}", handle.stats());
    }
}

fn client(mut args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut camera = None;
    if args.first().is_some_and(|a| a == "--camera") {
        camera = args.get(1).cloned();
        args = args.get(2..).unwrap_or_default();
    }
    let mut req = match args.first().map(String::as_str) {
        Some("rgb") => Frames::rgb(),
        Some("nv12") => Frames::nv12(),
        Some("mjpg") => Frames::formats([FourCc::MJPG]),
        Some("h264") => Frames::formats([FourCc::H264]),
        _ => Frames::gray(),
    };
    if let Some((w, h)) = args.get(1).and_then(|s| s.split_once('x')) {
        req = req.size(w.parse()?, h.parse()?);
    }
    let seconds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let client = match &camera {
        Some(camera) => FrameClient::request_camera(socket_path(), camera, &req)?,
        None => FrameClient::request(socket_path(), &req)?,
    }
    .reconnecting();
    print!("{}", client.plan().unwrap_or_default());

    let started = Instant::now();
    let cpu_start = cpu_ms();
    let (mut frames, mut keyframes, mut bytes, mut size, mut transport) =
        (0u32, 0u32, 0usize, (0, 0), "");
    let mut ages = Vec::new();
    while started.elapsed() < Duration::from_secs(seconds) {
        let RecvOutcome::Data(frame) = client.recv(Duration::from_millis(500)) else {
            continue;
        };
        frames += 1;
        keyframes += u32::from(!frame.meta().delta);
        bytes += frame.planes().iter().map(|p| p.data().len()).sum::<usize>();
        let res = frame.meta().format.resolution;
        size = (res.width.get(), res.height.get());
        transport = match frame.meta().residency {
            Some(FrameResidency::Dmabuf) => "dma-buf",
            _ => "memfd",
        };
        if let Some(now) = frame.meta().clock.and_then(|c| c.now_ns()) {
            ages.push(now.saturating_sub(frame.meta().timestamp) as f64 / 1e6);
        }
    }
    ages.sort_by(f64::total_cmp);
    let secs = started.elapsed().as_secs_f64();
    println!(
        "{frames} frames {}x{} via {transport}: {:.1} fps, {keyframes} keyframes, {:.0} kbit/s, \
         age p50 {:.1} ms, CPU {:.1}%",
        size.0,
        size.1,
        f64::from(frames) / secs,
        bytes as f64 * 8.0 / secs / 1000.0,
        ages.get(ages.len() / 2).copied().unwrap_or(0.0),
        (cpu_ms() - cpu_start) / secs / 10.0
    );
    Ok(())
}

/// CPU time of this process in milliseconds.
fn cpu_ms() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let fields: Vec<&str> = stat
        .rsplit(')')
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect();
    let ticks = |i: usize| {
        fields
            .get(i)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    (ticks(11) + ticks(12)) * 10.0
}
