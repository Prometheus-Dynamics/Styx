//! One camera, many processes. Run the service once, then any number of clients, each asking for
//! the frames it needs; the service plans one shared capture for all of them and passes frames as
//! file descriptors (camera buffers, memfds), without copying.
//!
//! ```text
//! camera_service serve [camera name substring]          # socket: $STYX_SOCKET or /tmp/styx-camera.sock
//! camera_service client luma [WxH] [seconds]            # also: rgb, nv12, mjpg
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
        Some("client") => client(&args[1..]),
        _ => Err(
            "usage: camera_service serve [camera] | client <luma|rgb|nv12|mjpg> [WxH] [seconds]"
                .into(),
        ),
    }
}

fn serve(name: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let device = probe_all()
        .into_iter()
        .find(|d| name.is_none_or(|n| d.identity.display.contains(n)))
        .ok_or("no camera found")?;
    println!("serving {} on {}", device.identity.display, socket_path());
    let service = CameraService::new(device).serve(socket_path())?;
    let mut shown = None;
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let plan = service.plan();
        if plan != shown {
            println!("{}", plan.as_deref().unwrap_or("(no capture)\n"));
            shown = plan;
        }
        println!("{:?}", service.stats());
    }
}

fn client(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut req = match args.first().map(String::as_str) {
        Some("rgb") => FrameRequirements::formats([FourCc::RG24]),
        Some("nv12") => FrameRequirements::formats([FourCc::NV12]),
        Some("mjpg") => FrameRequirements::formats([FourCc::MJPG]),
        _ => FrameRequirements::luma(),
    };
    if let Some((w, h)) = args.get(1).and_then(|s| s.split_once('x')) {
        req = req.output_resolution(w.parse()?, h.parse()?);
    }
    let seconds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let client = FrameClient::request(socket_path(), &req)?;
    print!("{}", client.plan().unwrap_or_default());

    let started = Instant::now();
    let cpu_start = cpu_ms();
    let (mut frames, mut size, mut transport) = (0u32, (0, 0), "");
    let mut ages = Vec::new();
    while started.elapsed() < Duration::from_secs(seconds) {
        let RecvOutcome::Data(frame) = client.recv(Duration::from_millis(500)) else {
            continue;
        };
        frames += 1;
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
        "{frames} frames {}x{} via {transport}: {:.1} fps, age p50 {:.1} ms, CPU {:.1}%",
        size.0,
        size.1,
        f64::from(frames) / secs,
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
