//! A long run of a camera served to clients, for leaks and drift: a camera service with one
//! vision client pulling every frame, a second client joining and leaving (a restart each
//! way), and the exposure set and released, over and over. Every 10 s it samples the
//! process's resident memory, open descriptors and threads, and the client's frames, gaps and
//! timestamp step; at the end, each one's growth per hour and per million frames (a straight
//! line fitted after the first minute, which warms the pools up, through the samples taken
//! while the second client has been away for a few seconds).
//!
//! On the PC with a simulated camera, or on a device with its camera (the first native one,
//! else the first camera):
//!
//! ```text
//! STYX_SIM_CAMERAS=ov9782 soak [minutes] [fps] [lean]
//! soak 30 120
//! ```
//!
//! `lean` runs the camera with `StyxConfig::lean_memory` (a small device's buffers).

use std::time::{Duration, Instant};

use styx::capture_api::native_controls as ctl;
use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;

fn proc_status(key: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn open_fds() -> u64 {
    std::fs::read_dir("/proc/self/fd").map_or(0, |d| d.count() as u64)
}

struct Sample {
    secs: f64,
    frames: u64,
    rss_kib: u64,
    fds: u64,
    threads: u64,
    /// How long the second client had been away (`None`: joined).
    away: Option<Duration>,
}

/// How long after the second client leaves a sample counts as steady (the restart that
/// follows takes a fraction of a second on the CM5).
const SETTLED: Duration = Duration::from_secs(3);

/// Least-squares slope of `y` against `x`.
fn slope(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len() as f64;
    let (mx, my) = (x.iter().sum::<f64>() / n, y.iter().sum::<f64>() / n);
    let num: f64 = x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)).sum();
    let den: f64 = x.iter().map(|a| (a - mx).powi(2)).sum();
    if den == 0.0 { 0.0 } else { num / den }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let minutes: f64 = args.next().map_or(Ok(30.0), |a| a.parse())?;
    let fps: u32 = args.next().map_or(Ok(120), |a| a.parse())?;
    let lean = args.next().as_deref() == Some("lean");
    let cameras = styx::probe_all();
    let camera = cameras
        .iter()
        .find(|d| d.backends.iter().any(|b| b.kind == BackendKind::Native))
        .or(cameras.first())
        .ok_or("no camera")?
        .clone();
    println!(
        "camera: {}, {minutes} min at {fps} fps{}",
        camera.identity.display,
        if lean { ", lean memory" } else { "" }
    );
    let path = std::env::temp_dir().join(format!("styx-soak-{}.sock", std::process::id()));
    let mut service = CameraService::new(camera).keep_streaming();
    if lean {
        service = service.capture_config(StyxConfig::default().lean_memory());
    }
    let service = service.serve(&path)?;
    let vision = FrameClient::request(&path, &Frames::gray().size(640, 400).fps(fps))?;

    let start = Instant::now();
    let end = start + Duration::from_secs_f64(minutes * 60.0);
    let (mut frames, mut gaps, mut last_ts, mut max_step) = (0u64, 0u64, None, 0u64);
    let mut last_seq: Option<u64> = None;
    let mut samples: Vec<Sample> = Vec::new();
    let mut next_sample = start;
    let mut next_churn = start + Duration::from_secs(20);
    let mut second: Option<FrameClient> = None;
    let mut left_at = start;
    let mut churns = 0u32;
    while Instant::now() < end {
        if let RecvOutcome::Data(f) = vision.recv(Duration::from_millis(200)) {
            frames += 1;
            let ts = f.meta().timestamp;
            if let Some(prev) = last_ts {
                max_step = max_step.max(ts.saturating_sub(prev));
            }
            last_ts = Some(ts);
            if let Some(seq) = f.meta().sequence().map(u64::from) {
                if last_seq.is_some_and(|p| seq > p + 1) {
                    gaps += 1;
                }
                last_seq = Some(seq);
            }
        }
        if let Some(c) = &second {
            let _ = c.recv(Duration::ZERO);
        }
        let now = Instant::now();
        if now >= next_churn {
            next_churn = now + Duration::from_secs(20);
            churns += 1;
            // Join and leave a larger client (a restart each way), and set and release the
            // exposure.
            if second.take().is_some() {
                left_at = now;
            } else {
                match FrameClient::request(&path, &Frames::gray().size(1280, 800).fps(fps)) {
                    Ok(c) => second = Some(c),
                    Err(e) => println!("second client refused: {e}"),
                }
            }
            let _ = vision.set_control(ctl::EXPOSURE_TIME_US, ControlValue::Uint(2000));
            let _ = vision.set_control(ctl::AE_ENABLE, ControlValue::Bool(true));
            last_ts = None;
            last_seq = None;
        }
        if now >= next_sample {
            next_sample = now + Duration::from_secs(10);
            let s = Sample {
                secs: start.elapsed().as_secs_f64(),
                frames,
                rss_kib: proc_status("VmRSS:"),
                fds: open_fds(),
                threads: proc_status("Threads:"),
                away: second.is_none().then(|| now - left_at),
            };
            println!(
                "{:7.0} s  frames {:9}  rss {:7} KiB  fds {:4}  threads {:3}  gaps {gaps}  \
                 max step {:.1} ms  restarts {}",
                s.secs,
                s.frames,
                s.rss_kib,
                s.fds,
                s.threads,
                max_step as f64 / 1e6,
                service.stats().restarts
            );
            samples.push(s);
        }
    }
    drop(second);
    drop(vision);
    let restarts = service.stats().restarts;
    service.stop();

    // The same state each time (the second client away, and long enough for the restart that
    // gives its memory back to be over), so a capture's size is not a trend.
    let steady: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.secs >= 60.0 && s.away.is_some_and(|a| a >= SETTLED))
        .collect();
    if steady.len() < 3 {
        return Err("run too short for a slope (at least 2 minutes)".into());
    }
    let t: Vec<f64> = steady.iter().map(|s| s.secs / 3600.0).collect();
    let f: Vec<f64> = steady.iter().map(|s| s.frames as f64 / 1e6).collect();
    let rate = slope(&t, &f);
    for (name, y) in [
        (
            "rss KiB",
            steady.iter().map(|s| s.rss_kib as f64).collect::<Vec<_>>(),
        ),
        ("fds", steady.iter().map(|s| s.fds as f64).collect()),
        ("threads", steady.iter().map(|s| s.threads as f64).collect()),
    ] {
        let per_hour = slope(&t, &y);
        println!(
            "{name}: {:.0} .. {:.0}; {per_hour:+.2} per hour, {:+.2} per million frames",
            y.iter().cloned().fold(f64::INFINITY, f64::min),
            y.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            per_hour / rate.max(f64::EPSILON)
        );
    }
    println!(
        "{frames} frames ({rate:.2} million per hour), {gaps} gaps, {churns} join/leave and \
         control rounds, {restarts} restarts"
    );
    Ok(())
}
