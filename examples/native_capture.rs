//! Native sensors through the normal Styx API: `probe_all` lists the bridged camera with its
//! modes (exact frame rate ranges), `CaptureRequest` and the planner start raw capture at chosen
//! rates, and exposure/gain/frame duration go through the control plane. Prints measured rates,
//! the open→first-frame latency, and CPU and memory use.
//!
//! ```sh
//! STYX_SENSOR_PATH=/tmp/styx-native/ov9782.toml native_capture [frames] [backend]
//! ```
//!
//! `backend` is `native` (default) or another backend kind (e.g. `libcamera`) to measure the
//! same start latency through it.

use std::time::{Duration, Instant};

use styx::capture_api::native_controls as ctl;
use styx::prelude::*;

fn cpu_seconds() -> f64 {
    // utime + stime of this process, in clock ticks (100 Hz).
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            let rest = s.rsplit_once(')')?.1.split_whitespace().collect::<Vec<_>>();
            let u: f64 = rest.get(11)?.parse().ok()?;
            let k: f64 = rest.get(12)?.parse().ok()?;
            Some((u + k) / 100.0)
        })
        .unwrap_or(0.0)
}

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

/// Receives `n` frames; returns the rate from their sequence numbers and timestamps, and the
/// last frame.
fn measure(n: usize, mut recv: impl FnMut() -> Option<FrameLease>) -> (f64, Option<FrameLease>) {
    let mut first: Option<(u32, u64)> = None;
    let mut last = (0u32, 0u64);
    let mut keep = None;
    for i in 0..n {
        let Some(f) = recv() else {
            println!("  no frame after {i}");
            break;
        };
        let seq = f.meta().sequence().unwrap_or(0);
        let ts = f.meta().timestamp;
        if first.is_none() && i > 0 {
            first = Some((seq, ts));
        }
        last = (seq, ts);
        keep = Some(f);
    }
    let (s0, t0) = first.unwrap_or(last);
    let fps = f64::from(last.0.wrapping_sub(s0)) / ((last.1.saturating_sub(t0)) as f64 / 1e9);
    (fps, keep)
}

fn recv(handle: &CaptureHandle) -> Option<FrameLease> {
    match handle.recv_blocking(Duration::from_secs(2)) {
        RecvOutcome::Data(f) => Some(f),
        _ => None,
    }
}

fn main() -> Result<(), CaptureError> {
    let args: Vec<String> = std::env::args().collect();
    let frames: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(120);
    let kind: BackendKind = args
        .get(2)
        .and_then(|a| a.parse().ok())
        .unwrap_or(BackendKind::Native);
    let t = Instant::now();
    let probe = styx::probe_all_with_errors_with_config(&StyxConfig::default());
    println!("probe_all: {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
    for e in &probe.errors {
        println!("  probe error: {e}");
    }
    for d in &probe.devices {
        println!("{} keys {:?}", d.identity.display, d.identity.keys);
        for b in &d.backends {
            println!("  backend {}", b.kind);
            for m in &b.descriptor.modes {
                let sw = m.interval_stepwise;
                println!(
                    "    {} {}x{}: intervals {:?}, range {}",
                    m.format.code,
                    m.format.resolution.width,
                    m.format.resolution.height,
                    m.intervals
                        .iter()
                        .map(|i| format!(
                            "{}/{} s ({:.3} fps)",
                            i.numerator,
                            i.denominator,
                            i.fps()
                        ))
                        .collect::<Vec<_>>(),
                    sw.map_or("-".into(), |s| format!(
                        "{:.3}..{:.3} fps ({}/{}..{}/{} s)",
                        s.max.fps(),
                        s.min.fps(),
                        s.max.numerator,
                        s.max.denominator,
                        s.min.numerator,
                        s.min.denominator
                    ))
                );
            }
            for c in &b.descriptor.controls {
                println!(
                    "    control {:#x} {} {:?}..{:?}",
                    c.id.0, c.name, c.min, c.max
                );
            }
        }
    }
    let Some(device) = probe.devices.iter().find(|d| d.backend(kind).is_some()) else {
        println!("no {kind} camera");
        return Ok(());
    };
    let backend = device.backend(kind).expect("found");
    let mode = backend.descriptor.modes[0].clone();
    println!(
        "\nusing {} via {kind}, mode {} {}x{}",
        device.identity.display,
        mode.format.code,
        mode.format.resolution.width,
        mode.format.resolution.height
    );

    // CaptureRequest at 30/60/120 fps.
    for fps in [30u32, 60, 120] {
        let (cpu0, t0) = (cpu_seconds(), Instant::now());
        let handle = CaptureRequest::new(device)
            .backend(kind)
            .mode(mode.id.clone())
            .interval(Interval::from_fps(fps).expect("fps"))
            .start()?;
        let started = t0.elapsed();
        let RecvOutcome::Data(first) = handle.recv_blocking(Duration::from_secs(3)) else {
            panic!("no first frame: {:?}", handle.last_error());
        };
        let latency = t0.elapsed();
        drop(first);
        let (measured, keep) = measure(frames, || recv(&handle));
        let wall = t0.elapsed().as_secs_f64();
        let f = keep.expect("frames");
        println!(
            "request {fps} fps: start() {:.1} ms, open->first frame {:.1} ms, measured {measured:.3} fps ({:+.3}%), CPU {:.1}% of one core, RSS {} KiB, residency {:?}, exportable {}, native meta {:?}",
            started.as_secs_f64() * 1e3,
            latency.as_secs_f64() * 1e3,
            (measured - f64::from(fps)) / f64::from(fps) * 100.0,
            (cpu_seconds() - cpu0) / wall * 100.0,
            rss_kib(),
            f.meta().residency,
            f.export_backing().is_ok(),
            f.meta().native(),
        );
        drop(keep);
        if kind == BackendKind::Native && fps == 60 {
            controls(&handle)?;
        }
        handle.stop();
    }

    // The planner: raw frames at exactly 30, 60 and 120 fps.
    for fps in [30u32, 60, 120] {
        let req = FrameRequirements::formats([mode.format.code])
            .min_fps(fps)
            .priority(Priority::Power);
        match styx::planner::plan_best(&probe.devices, &req) {
            Ok(plan) => {
                print!("\n{plan}");
                let t0 = Instant::now();
                let mut planned = plan.start()?;
                let (measured, _) = measure(frames, || {
                    match planned.next_frame(Duration::from_secs(2)) {
                        RecvOutcome::Data(f) => Some(f),
                        _ => None,
                    }
                });
                println!(
                    "planned {fps} fps: measured {measured:.3} fps ({:+.3}%), {:.1} ms to {frames} frames",
                    (measured - f64::from(fps)) / f64::from(fps) * 100.0,
                    t0.elapsed().as_secs_f64() * 1e3
                );
                planned.stop();
            }
            Err(e) => println!("\nplanner {fps} fps: {e}"),
        }
    }
    Ok(())
}

fn controls(handle: &CaptureHandle) -> Result<(), CaptureError> {
    println!(
        "  controls now: exposure {:?} us, gain {:?}, frame {:?} us, rate {:?}",
        handle.get_control(ctl::EXPOSURE_TIME_US)?,
        handle.get_control(ctl::GAIN)?,
        handle.get_control(ctl::FRAME_DURATION_US)?,
        handle.get_control(ctl::FRAME_RATE)?
    );
    handle.set_control(ctl::EXPOSURE_TIME_US, ControlValue::Uint(8000))?;
    handle.set_control(ctl::GAIN, ControlValue::Float(2.5))?;
    handle.set_control(ctl::FRAME_RATE, ControlValue::Float(45.0))?;
    let t0 = Instant::now();
    let mut applied_at = None;
    while t0.elapsed() < Duration::from_secs(1) {
        let RecvOutcome::Data(f) = handle.recv_blocking(Duration::from_secs(1)) else {
            break;
        };
        if let Some(n) = f.meta().native() {
            if applied_at.is_none() && n.exposure_ns.abs_diff(8_000_000) < 20_000 {
                applied_at = Some(n.sequence);
                println!(
                    "  frame {} carries exposure {:.3} ms gain {:.3} frame {:.3} ms",
                    n.sequence,
                    n.exposure_ns as f64 / 1e6,
                    n.gain(),
                    n.frame_duration_ns as f64 / 1e6
                );
            }
        }
    }
    println!(
        "  controls after: exposure {:?} us, gain {:?}, rate {:?}",
        handle.get_control(ctl::EXPOSURE_TIME_US)?,
        handle.get_control(ctl::GAIN)?,
        handle.get_control(ctl::FRAME_RATE)?
    );
    Ok(())
}
