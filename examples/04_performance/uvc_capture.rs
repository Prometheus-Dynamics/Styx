//! USB cameras through the userspace UVC backend (`BackendKind::Uvc`) and through `uvcvideo`
//! (`BackendKind::V4l2`), side by side: frame rate, timestamp jitter, delivery latency and CPU,
//! controls, and unplug/replug.
//!
//! ```sh
//! uvc_capture list
//! uvc_capture capture <uvc|v4l2> <YUYV|MJPG> <WxH> <fps> [frames]
//! uvc_capture controls <uvc|v4l2>
//! uvc_capture replug <uvc|v4l2>          # toggles the camera's sysfs `authorized` (root)
//! ```
//!
//! The userspace backend needs the camera's video interfaces free: unbind `uvcvideo`, or set
//! `STYX_UVC_DETACH=1` to let Styx detach it for the capture (it is rebound afterwards).

use std::time::{Duration, Instant};

use styx::prelude::*;

/// This process's (user, system) CPU seconds.
fn cpu_seconds() -> (f64, f64) {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            let rest = s.rsplit_once(')')?.1.split_whitespace().collect::<Vec<_>>();
            let u: f64 = rest.get(11)?.parse().ok()?;
            let k: f64 = rest.get(12)?.parse().ok()?;
            Some((u / 100.0, k / 100.0))
        })
        .unwrap_or((0.0, 0.0))
}

/// CPU time (ms) the scheduler accounted to every thread under `/proc/<pid>/task` for the
/// given pids (`se.sum_exec_runtime`: exact, unlike tick-sampled `utime`/`stime`); `None`
/// pids means every process (kernel workers included).
fn runtime_ms(pid: Option<u32>) -> f64 {
    let pids: Vec<std::path::PathBuf> = match pid {
        Some(p) => vec![format!("/proc/{p}").into()],
        None => std::fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().parse::<u32>().is_ok())
            })
            .collect(),
    };
    let mut total = 0.0;
    for p in pids {
        for t in std::fs::read_dir(p.join("task"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let s = std::fs::read_to_string(t.path().join("sched")).unwrap_or_default();
            total += s
                .lines()
                .find(|l| l.starts_with("se.sum_exec_runtime"))
                .and_then(|l| l.rsplit(':').next()?.trim().parse::<f64>().ok())
                .unwrap_or(0.0);
        }
    }
    total
}

/// Minor page faults of this process.
fn minor_faults() -> u64 {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            s.rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(7)?
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

/// Busy CPU seconds of the whole system (all cores; kernel workers included).
fn system_busy_seconds() -> f64 {
    std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| {
            let v: Vec<f64> = s
                .lines()
                .next()?
                .split_whitespace()
                .skip(1)
                .filter_map(|x| x.parse().ok())
                .collect();
            // user nice system idle iowait irq softirq steal
            let total: f64 = v.iter().take(8).sum();
            Some((total - v.get(3)? - v.get(4)?) / 100.0)
        })
        .unwrap_or(0.0)
}

fn mono_ns() -> u64 {
    TimestampClock::Monotonic.now_ns().unwrap_or(0)
}

fn stats(v: &[f64]) -> String {
    if v.is_empty() {
        return "-".into();
    }
    let n = v.len() as f64;
    let mean = v.iter().sum::<f64>() / n;
    let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let p = |q: f64| s[((n - 1.0) * q).round() as usize];
    format!(
        "mean {mean:.3} sd {sd:.3} min {:.3} p50 {:.3} p99 {:.3} max {:.3}",
        s[0],
        p(0.5),
        p(0.99),
        s[s.len() - 1]
    )
}

fn find(kind: BackendKind) -> Option<ProbedDevice> {
    let probe = styx::probe_all_with_errors_with_config(&StyxConfig::default());
    for e in &probe.errors {
        println!("probe error: {e}");
    }
    probe
        .devices
        .into_iter()
        .find(|d| d.backend(kind).is_some() && d.backend(BackendKind::Uvc).is_some())
}

fn list() {
    let probe = styx::probe_all_with_errors_with_config(&StyxConfig::default());
    for e in &probe.errors {
        println!("probe error: {e}");
    }
    for d in &probe.devices {
        println!("{} keys {:?}", d.identity.display, d.identity.keys);
        for b in &d.backends {
            println!("  backend {} {:?}", b.kind, b.properties);
            for m in &b.descriptor.modes {
                println!(
                    "    {} {}x{} {:?}",
                    m.format.code,
                    m.format.resolution.width,
                    m.format.resolution.height,
                    m.intervals.iter().map(|i| i.fps()).collect::<Vec<_>>()
                );
            }
            for c in &b.descriptor.controls {
                println!(
                    "    control {:#x} {} {:?}..{:?} default {:?}",
                    c.id.0, c.name, c.min, c.max, c.default
                );
            }
        }
    }
}

fn start(
    device: &ProbedDevice,
    kind: BackendKind,
    code: FourCc,
    w: u32,
    h: u32,
    fps: u32,
) -> Result<CaptureHandle, CaptureError> {
    let backend = device.backend(kind).expect("backend");
    let mode = backend
        .descriptor
        .modes
        .iter()
        .find(|m| {
            m.format.code == code
                && m.format.resolution.width.get() == w
                && m.format.resolution.height.get() == h
        })
        .ok_or_else(|| CaptureError::InvalidConfig(format!("no {code} {w}x{h}")))?;
    CaptureRequest::new(device)
        .backend(kind)
        .mode(mode.id.clone())
        .interval(Interval::from_fps(fps).expect("fps"))
        .start()
}

fn capture(kind: BackendKind, code: FourCc, w: u32, h: u32, fps: u32, frames: usize) {
    let Some(device) = find(kind) else {
        println!("no camera with a {kind} backend");
        return;
    };
    let t0 = Instant::now();
    let handle = match start(&device, kind, code, w, h, fps) {
        Ok(h) => h,
        Err(e) => {
            println!("start failed: {e}");
            return;
        }
    };
    let started = t0.elapsed();
    let RecvOutcome::Data(first) = handle.recv_blocking(Duration::from_secs(5)) else {
        println!("no first frame: {:?}", handle.last_error());
        return;
    };
    let first_frame = t0.elapsed();
    // UVC_FIXED_RATE=1: exposure may not lower the frame rate (`exposure_dynamic_framerate`
    // off), so both backends run at the requested rate whatever the light.
    if std::env::var_os("UVC_FIXED_RATE").is_some() {
        let r = handle.set_control(ControlId(0x009a_0903), ControlValue::Int(0));
        println!("exposure_dynamic_framerate off: {r:?}");
    }
    drop(first);
    // Settle 30 frames (AE, timestamp fits), then measure.
    for _ in 0..30 {
        let _ = handle.recv_blocking(Duration::from_secs(2));
    }
    let (cpu0, sys0, w0) = (cpu_seconds(), system_busy_seconds(), Instant::now());
    let faults0 = minor_faults();
    let (run0, all0) = (runtime_ms(Some(std::process::id())), runtime_ms(None));
    let (mut ts, mut arrival, mut latency, mut arrival_latency, mut end_latency) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut seq_first, mut seq_last, mut damaged, mut from_pts) = (None, 0u32, 0, 0);
    let mut bytes = 0usize;
    for _ in 0..frames {
        let RecvOutcome::Data(f) = handle.recv_blocking(Duration::from_secs(2)) else {
            println!("frame missing: {:?}", handle.last_error());
            break;
        };
        let now = mono_ns();
        let m = f.meta();
        latency.push((now.saturating_sub(m.timestamp)) as f64 / 1e6);
        ts.push(m.timestamp);
        let seq = m.sequence().unwrap_or(0);
        seq_first.get_or_insert(seq);
        seq_last = seq;
        bytes += f.payload_bytes();
        if let Some(u) = m.uvc() {
            arrival.push(u.first_payload_ns);
            arrival_latency.push((now.saturating_sub(u.first_payload_ns)) as f64 / 1e6);
            end_latency.push((now.saturating_sub(u.last_payload_ns)) as f64 / 1e6);
            damaged += usize::from(u.error);
            from_pts += usize::from(u.timestamp_from_pts);
        } else if let Some(v) = m.v4l2() {
            damaged += usize::from(v.flags & 0x40 != 0);
        }
    }
    let wall = w0.elapsed().as_secs_f64();
    let cpu1 = cpu_seconds();
    let (user, kernel) = (
        (cpu1.0 - cpu0.0) / wall * 100.0,
        (cpu1.1 - cpu0.1) / wall * 100.0,
    );
    let cpu = user + kernel;
    let system = (system_busy_seconds() - sys0) / wall * 100.0;
    let run = (runtime_ms(Some(std::process::id())) - run0) / 1e3 / wall * 100.0;
    let all = (runtime_ms(None) - all0) / 1e3 / wall * 100.0;
    let deltas = |t: &[u64]| {
        t.windows(2)
            .map(|p| (p[1] as f64 - p[0] as f64) / 1e6)
            .collect::<Vec<_>>()
    };
    let n = ts.len();
    let span = (ts.last().copied().unwrap_or(0) - ts.first().copied().unwrap_or(0)) as f64 / 1e9;
    println!(
        "{kind} {code} {w}x{h}@{fps}: start() {:.1} ms, first frame {:.1} ms; {n} frames in {wall:.2} s = {:.3} fps (timestamps {:.3} fps); sequence gaps {}; damaged {damaged}; {:.0} KiB/frame; CPU {cpu:.2}% of a core (user {user:.2}, kernel {kernel:.2}); whole system {system:.1}% of a core; scheduler runtime: process {run:.2}%, all tasks {all:.2}% of a core; {:.1} page faults/frame",
        started.as_secs_f64() * 1e3,
        first_frame.as_secs_f64() * 1e3,
        n as f64 / wall,
        (n.saturating_sub(1)) as f64 / span.max(1e-9),
        (seq_last.wrapping_sub(seq_first.unwrap_or(0)) as usize + 1).saturating_sub(n),
        bytes as f64 / n.max(1) as f64 / 1024.0,
        (minor_faults() - faults0) as f64 / n.max(1) as f64,
    );
    if std::env::var_os("UVC_THREADS").is_some() {
        for t in std::fs::read_dir("/proc/self/task")
            .into_iter()
            .flatten()
            .flatten()
        {
            let s = std::fs::read_to_string(t.path().join("stat")).unwrap_or_default();
            if let Some((name, rest)) = s.split_once(" (").and_then(|(_, r)| r.rsplit_once(')')) {
                let f: Vec<&str> = rest.split_whitespace().collect();
                println!("  thread {name}: utime {} stime {}", f[11], f[12]);
            }
        }
    }
    println!("  frame interval (timestamps), ms: {}", stats(&deltas(&ts)));
    println!(
        "  delivery latency (now - timestamp), ms: {}",
        stats(&latency)
    );
    if !arrival.is_empty() {
        println!("  timestamps from PTS: {from_pts}/{n}");
        println!(
            "  frame interval (first payload arrival), ms: {}",
            stats(&deltas(&arrival))
        );
        println!(
            "  delivery latency (now - first payload), ms: {}",
            stats(&arrival_latency)
        );
        println!(
            "  delivery latency (now - last payload), ms: {}",
            stats(&end_latency)
        );
    }
    handle.stop();
}

fn controls(kind: BackendKind) {
    let Some(device) = find(kind) else {
        println!("no camera with a {kind} backend");
        return;
    };
    let handle = match start(&device, kind, FourCc::new(*b"YUYV"), 640, 480, 30) {
        Ok(h) => h,
        Err(e) => {
            println!("start failed: {e}");
            return;
        }
    };
    let _ = handle.recv_blocking(Duration::from_secs(5));
    let metas = device
        .backend(kind)
        .expect("backend")
        .descriptor
        .controls
        .clone();
    for c in &metas {
        println!(
            "{:#x} {:<28} {:?}..{:?} default {:?} now {:?}",
            c.id.0,
            c.name,
            c.min,
            c.max,
            c.default,
            handle.get_control(c.id)
        );
    }
    // Brightness, manual exposure, gain, power line frequency; read back; restore.
    let ids = [
        (0x0098_0900u32, ControlValue::Int(200)),
        (0x009a_0901, ControlValue::Uint(1)),
        (0x009a_0902, ControlValue::Int(100)),
        (0x0098_0913, ControlValue::Int(50)),
        (0x0098_0918, ControlValue::Uint(1)),
    ];
    let mean_luma = |handle: &CaptureHandle| {
        let mut sum = 0.0;
        for _ in 0..10 {
            if let RecvOutcome::Data(f) = handle.recv_blocking(Duration::from_secs(2)) {
                let planes = f.planes();
                let d = planes[0].data();
                sum += d.iter().step_by(2).map(|&v| f64::from(v)).sum::<f64>()
                    / (d.len() / 2).max(1) as f64;
            }
        }
        sum / 10.0
    };
    println!("mean luma before: {:.1}", mean_luma(&handle));
    for (id, v) in ids {
        let id = ControlId(id);
        let before = handle.get_control(id);
        let set = handle.set_control(id, v.clone());
        let after = handle.get_control(id);
        println!("{:#x}: {before:?} -> set {v:?}: {set:?} -> {after:?}", id.0);
    }
    println!("mean luma after: {:.1}", mean_luma(&handle));
    // Exposure 10x longer: brighter.
    let _ = handle.set_control(ControlId(0x009a_0902), ControlValue::Int(1000));
    println!("mean luma at exposure 1000: {:.1}", mean_luma(&handle));
    for c in &metas {
        let _ = handle.set_control(c.id, c.default.clone());
    }
    handle.stop();
}

fn authorize(port: &str, on: bool) {
    let path = format!("/sys/bus/usb/devices/{port}/authorized");
    if let Err(e) = std::fs::write(&path, if on { "1" } else { "0" }) {
        println!("writing {path}: {e}");
    }
}

fn replug(kind: BackendKind) {
    let Some(device) = find(kind) else {
        println!("no camera with a {kind} backend");
        return;
    };
    let port = device
        .backend(BackendKind::Uvc)
        .and_then(|b| b.properties.iter().find(|(k, _)| k == "usb_port"))
        .map(|(_, v)| v.clone())
        .expect("usb port");
    let backend = device.backend(kind).expect("backend");
    let mode = backend
        .descriptor
        .modes
        .iter()
        .find(|m| m.format.code == FourCc::new(*b"YUYV") && m.format.resolution.width.get() == 640)
        .expect("640 YUYV")
        .clone();
    let handle = CaptureRequest::new(&device)
        .backend(kind)
        .mode(mode.id.clone())
        .interval(Interval::from_fps(30).expect("fps"))
        .start_with_policy(CaptureStartPolicy::resilient());
    let handle = match handle {
        Ok(h) => h,
        Err(e) => {
            println!("start failed: {e}");
            return;
        }
    };
    let count = |handle: &CaptureHandle, d: Duration| {
        let t = Instant::now();
        let mut n = 0;
        while t.elapsed() < d {
            if let RecvOutcome::Data(_) = handle.recv_blocking(Duration::from_millis(200)) {
                n += 1;
            }
        }
        n
    };
    println!(
        "before: {} frames in 2 s",
        count(&handle, Duration::from_secs(2))
    );
    let t = Instant::now();
    authorize(&port, false);
    println!(
        "deauthorized {port}: {} frames in the next 2 s",
        count(&handle, Duration::from_secs(2))
    );
    authorize(&port, true);
    let t1 = Instant::now();
    let mut back = None;
    while t1.elapsed() < Duration::from_secs(15) {
        if let RecvOutcome::Data(_) = handle.recv_blocking(Duration::from_millis(200)) {
            back = Some(t1.elapsed());
            break;
        }
    }
    println!(
        "reauthorized: first frame after {:?} (total outage {:.1} s); then {} frames in 2 s; last error {:?}",
        back,
        t.elapsed().as_secs_f64(),
        count(&handle, Duration::from_secs(2)),
        handle.last_error()
    );
    handle.stop();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let kind = |i: usize| {
        args.get(i)
            .and_then(|a| a.parse::<BackendKind>().ok())
            .unwrap_or(BackendKind::Uvc)
    };
    match args.get(1).map(String::as_str) {
        Some("capture") => {
            let code = args.get(3).map_or(*b"YUYV", |s| {
                let mut c = [b' '; 4];
                c.copy_from_slice(&format!("{s:<4}").as_bytes()[..4]);
                c
            });
            let (w, h) = args
                .get(4)
                .and_then(|s| s.split_once('x'))
                .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                .unwrap_or((640, 480));
            let fps = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(30);
            let frames = args.get(6).and_then(|s| s.parse().ok()).unwrap_or(300);
            capture(kind(2), FourCc::new(code), w, h, fps, frames);
        }
        Some("controls") => controls(kind(2)),
        Some("replug") => replug(kind(2)),
        _ => list(),
    }
}
