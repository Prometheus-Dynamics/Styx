//! On-device robustness checks for `styx-native`: discovery, hotplug, the exclusive open,
//! restarts with frames held, and a soak run with leak and timing numbers.
//!
//! ```sh
//! native_harden probe                 # cameras and problems, bridge state
//! native_harden hotplug <secs>        # print the provider's hotplug events
//! native_harden hold <secs> [fps]     # open, stream, report (Busy when another holds it)
//! native_harden restart <cycles>      # stop/start and close/reopen with frames held
//! native_harden soak <minutes>        # continuous capture with rate/exposure changes,
//!                                     # restarts and reopens; resource and timing report
//! ```
//!
//! Set `STYX_SENSOR_PATH` to the directory or file with the sensor description.

use std::time::{Duration, Instant};

use styx_graph::Provider;
use styx_native::{
    CameraInfo, NativeCamera, NativeError, NativeFrame, NativeProvider, SensorLibrary,
    StreamSettings,
};

const WAIT: Duration = Duration::from_secs(2);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let num = |i: usize, d: u64| -> u64 { args.get(i).and_then(|a| a.parse().ok()).unwrap_or(d) };
    let r = match args.first().map(String::as_str) {
        Some("probe") => probe(),
        Some("hotplug") => hotplug(num(1, 30)),
        Some("hold") => hold(num(1, 10), num(2, 60) as u32),
        Some("restart") => restart(num(1, 20) as usize),
        Some("soak") => soak(num(1, 30), num(2, 60)),
        _ => {
            eprintln!(
                "usage: native_harden probe|hotplug <s>|hold <s> [fps]|restart <n>|soak <min>"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        println!("error: {e} (disconnect: {})", e.is_disconnect());
        std::process::exit(1);
    }
}

fn stamp() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", t.as_secs(), t.subsec_millis())
}

fn first_camera() -> Result<CameraInfo, NativeError> {
    let (mut cams, problems) = styx_native::discover(&SensorLibrary::system());
    for p in problems {
        println!("problem: {p}");
    }
    if cams.is_empty() {
        return Err(NativeError::Topology("no bridged camera".into()));
    }
    Ok(cams.remove(0))
}

fn bridge_state(info: &CameraInfo) -> String {
    match styx_kernel::bus::SensorBridge::open(&info.location.subdev) {
        Ok(b) => format!(
            "bridge {}: state {:?}, power {:?}",
            info.location.subdev.display(),
            b.stream_state(),
            b.power()
        ),
        Err(e) => format!("bridge {}: {e}", info.location.subdev.display()),
    }
}

fn probe() -> Result<(), NativeError> {
    let t = Instant::now();
    let (cams, problems) = styx_native::discover(&SensorLibrary::system());
    println!(
        "{} discover: {} camera(s), {} problem(s) in {:.1} ms",
        stamp(),
        cams.len(),
        problems.len(),
        t.elapsed().as_secs_f64() * 1e3
    );
    for p in problems {
        println!("  problem: {p}");
    }
    for c in &cams {
        println!("  {} [{}] modes {}", c.display_name(), c.key, c.modes.len());
        println!("  {}", bridge_state(c));
    }
    Ok(())
}

fn hotplug(secs: u64) -> Result<(), NativeError> {
    let p = NativeProvider::new(SensorLibrary::system())
        .with_hotplug_interval(Duration::from_millis(200));
    let known = p
        .discover()
        .map_err(|e| NativeError::Topology(e.to_string()))?;
    println!("{} start: {} camera(s)", stamp(), known.len());
    let mut events = p.hotplug();
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match styx_graph::rt::block_on(styx_graph::rt::timeout(
            left,
            styx_graph::rt::next(&mut events),
        )) {
            Ok(Some(styx_graph::HotplugEvent::Added(d))) => {
                println!("{} added {} ({})", stamp(), d.key.0, d.name)
            }
            Ok(Some(styx_graph::HotplugEvent::Removed(k))) => {
                println!("{} removed {}", stamp(), k.0)
            }
            Ok(None) | Err(_) => break,
        }
    }
    println!("{} end", stamp());
    Ok(())
}

fn hold(secs: u64, fps: u32) -> Result<(), NativeError> {
    let info = first_camera()?;
    let t = Instant::now();
    let mut cam = match NativeCamera::open(info, Default::default()) {
        Ok(c) => c,
        Err(e @ NativeError::Busy(_)) => {
            println!("{} open: {e} after {:?}", stamp(), t.elapsed());
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    println!(
        "{} open: ok in {:?} (pid {})",
        stamp(),
        t.elapsed(),
        std::process::id()
    );
    cam.configure(&StreamSettings::new(1280, 800).fps(fps))?;
    let mut stream = cam.start()?;
    let end = Instant::now() + Duration::from_secs(secs);
    let mut n = 0u64;
    let mut last_report = Instant::now();
    while Instant::now() < end {
        match stream.next_blocking(WAIT) {
            Ok(Some(_)) => n += 1,
            Ok(None) => {
                println!("{} stream ended", stamp());
                break;
            }
            Err(e) => {
                println!(
                    "{} stream error after {n} frames: {e} (disconnect {})",
                    stamp(),
                    e.is_disconnect()
                );
                break;
            }
        }
        if last_report.elapsed() >= Duration::from_secs(5) {
            println!("{} {n} frames {:?}", stamp(), stream.stats());
            last_report = Instant::now();
        }
    }
    println!("{} done: {n} frames {:?}", stamp(), stream.stats());
    drop(stream);
    let r = cam.close();
    println!("{} close: {r:?}", stamp());
    Ok(())
}

fn checksum(f: &NativeFrame) -> u64 {
    f.data()
        .iter()
        .step_by(97)
        .fold(0xcbf29ce484222325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        })
}

fn restart(cycles: usize) -> Result<(), NativeError> {
    let info = first_camera()?;
    let mut cam = NativeCamera::open(info.clone(), Default::default())?;
    let settings = StreamSettings::new(1280, 800).fps(60);
    cam.configure(&settings)?;
    let mut held: Vec<(NativeFrame, u64)> = Vec::new();
    let t0 = Instant::now();
    for cycle in 0..cycles {
        let t = Instant::now();
        let mut stream = cam.start()?;
        let started = t.elapsed();
        let mut frames = 0;
        for _ in 0..10 {
            let f = stream.next_blocking(WAIT)?.expect("a frame");
            frames += 1;
            if held.len() < 8 && frames <= 2 {
                let sum = checksum(&f);
                held.push((f, sum));
            }
        }
        cam.stop()?;
        // Held frames keep their data after the stop (and across the next start).
        let intact = held.iter().all(|(f, s)| checksum(f) == *s);
        println!(
            "cycle {cycle}: start {:.1} ms, {frames} frames, {} held, intact {intact}, {:?}",
            started.as_secs_f64() * 1e3,
            held.len(),
            stream.stats()
        );
        if !intact {
            return Err(NativeError::State("a held frame changed"));
        }
        if cycle % 5 == 4 {
            // Close and reopen with the frames still held (a reconnect).
            cam.close()?;
            cam = NativeCamera::open(info.clone(), Default::default())?;
            cam.configure(&settings)?;
            println!("cycle {cycle}: reopened");
        }
        if held.len() >= 8 {
            held.drain(..4);
        }
    }
    println!("{cycles} cycles in {:.1} s", t0.elapsed().as_secs_f64());
    drop(held);
    cam.close()
}

/// Process resources: RSS kB, descriptors, dma-buf descriptors, device/dma-buf mappings.
fn resources() -> (u64, usize, usize, usize) {
    let rss = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0);
    let mut fds = 0;
    let mut dmabufs = 0;
    if let Ok(dir) = std::fs::read_dir("/proc/self/fd") {
        for e in dir.flatten() {
            fds += 1;
            if let Ok(t) = std::fs::read_link(e.path())
                && t.to_string_lossy().contains("dmabuf")
            {
                dmabufs += 1;
            }
        }
    }
    let maps = std::fs::read_to_string("/proc/self/maps")
        .map(|m| {
            m.lines()
                .filter(|l| l.contains("/dev/video") || l.contains("dmabuf"))
                .count()
        })
        .unwrap_or(0);
    (rss, fds, dmabufs, maps)
}

/// dma-bufs in the whole system, when the kernel exposes them.
fn system_dmabufs() -> Option<usize> {
    std::fs::read_dir("/sys/kernel/dmabuf/buffers")
        .ok()
        .map(|d| d.count())
}

fn monotonic() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes into the timespec we pass.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

#[derive(Default)]
struct Window {
    frames: u64,
    gaps: u64,
    errors: u64,
    latency_sum: f64,
    latency_max: f64,
    period_err_sum: f64,
    period_err_max: f64,
    periods: u64,
    first: Option<(u32, Duration)>,
    last: Option<(u32, Duration)>,
}

fn soak(minutes: u64, report_secs: u64) -> Result<(), NativeError> {
    let info = first_camera()?;
    let settings = StreamSettings::new(1280, 800).fps(60);
    let mut cam = NativeCamera::open(info.clone(), Default::default())?;
    cam.configure(&settings)?;
    let mut stream = cam.start()?;
    let rates = [30.0, 60.0, 120.0, 45.0, 90.0, 15.0];
    let exposures = [2_000u64, 8_000, 500, 4_000, 1_000];
    let t0 = Instant::now();
    let end = t0 + Duration::from_secs(minutes * 60);
    let mut next_change = t0 + Duration::from_secs(20);
    let mut next_restart = t0 + Duration::from_secs(120);
    let mut next_reopen = t0 + Duration::from_secs(600);
    let mut next_report = t0 + Duration::from_secs(report_secs);
    let (mut changes, mut restarts, mut reopens) = (0usize, 0u64, 0u64);
    let mut total = Window::default();
    let mut w = Window::default();
    let mut prev: Option<(u32, Duration, Duration)> = None;
    let base = resources();
    println!(
        "{} soak {minutes} min: start rss {} kB, fds {}, dmabuf fds {}, maps {}, system dmabufs {:?}",
        stamp(),
        base.0,
        base.1,
        base.2,
        base.3,
        system_dmabufs()
    );
    while Instant::now() < end {
        let f = match stream.next_blocking(WAIT) {
            Ok(Some(f)) => f,
            Ok(None) => return Err(NativeError::State("stream ended")),
            Err(e) => {
                println!("{} stream error: {e}", stamp());
                return Err(e);
            }
        };
        let now = monotonic();
        let latency = now.saturating_sub(f.timestamp).as_secs_f64() * 1e3;
        for win in [&mut w, &mut total] {
            win.frames += 1;
            win.errors += u64::from(f.error);
            win.latency_sum += latency;
            win.latency_max = win.latency_max.max(latency);
            win.first.get_or_insert((f.sequence, f.timestamp));
            win.last = Some((f.sequence, f.timestamp));
        }
        let duration = f.controls.map(|c| c.frame_duration).unwrap_or_default();
        if let Some((pseq, pts, pdur)) = prev {
            let gap = f.sequence.saturating_sub(pseq + 1);
            w.gaps += u64::from(gap);
            total.gaps += u64::from(gap);
            // Same period on both frames and no gap: the timestamp delta is one period.
            if gap == 0 && pdur == duration && !duration.is_zero() {
                let delta = f.timestamp.saturating_sub(pts).as_secs_f64();
                let err = (delta - duration.as_secs_f64()) / duration.as_secs_f64() * 1e6;
                for win in [&mut w, &mut total] {
                    win.period_err_sum += err;
                    win.period_err_max = win.period_err_max.max(err.abs());
                    win.periods += 1;
                }
            }
        }
        prev = Some((f.sequence, f.timestamp, duration));
        let now_i = Instant::now();
        if now_i >= next_change {
            let c = cam.controls();
            let fps = rates[changes % rates.len()];
            c.set_frame_rate(fps)?;
            c.set_exposure(Duration::from_micros(exposures[changes % exposures.len()]))?;
            changes += 1;
            next_change = now_i + Duration::from_secs(20);
        }
        if now_i >= next_report {
            report(t0, &w, &cam, &stream, restarts, reopens, changes);
            w = Window::default();
            next_report = now_i + Duration::from_secs(report_secs);
        }
        if now_i >= next_restart || now_i >= next_reopen {
            // Keep a frame across the restart.
            let keep = f;
            let sum = checksum(&keep);
            cam.stop()?;
            if now_i >= next_reopen {
                cam.close()?;
                cam = NativeCamera::open(info.clone(), Default::default())?;
                cam.configure(&settings)?;
                reopens += 1;
                next_reopen = now_i + Duration::from_secs(600);
            }
            stream = cam.start()?;
            if checksum(&keep) != sum {
                return Err(NativeError::State("a held frame changed across a restart"));
            }
            drop(keep);
            restarts += 1;
            prev = None;
            next_restart = now_i + Duration::from_secs(120);
        }
    }
    report(t0, &total, &cam, &stream, restarts, reopens, changes);
    drop(stream);
    cam.close()?;
    let fin = resources();
    println!(
        "{} soak done: rss {} -> {} kB, fds {} -> {}, dmabuf fds {} -> {}, maps {} -> {}, system dmabufs {:?}",
        stamp(),
        base.0,
        fin.0,
        base.1,
        fin.1,
        base.2,
        fin.2,
        base.3,
        fin.3,
        system_dmabufs()
    );
    Ok(())
}

fn report(
    t0: Instant,
    w: &Window,
    cam: &NativeCamera,
    stream: &styx_native::FrameStream,
    restarts: u64,
    reopens: u64,
    changes: usize,
) {
    let (rss, fds, dmabufs, maps) = resources();
    let fps = match (w.first, w.last) {
        (Some(a), Some(b)) if b.1 > a.1 => f64::from(b.0 - a.0) / (b.1 - a.1).as_secs_f64(),
        _ => 0.0,
    };
    let requested = cam
        .controls()
        .current()
        .map(|c| 1.0 / c.frame_duration.as_secs_f64())
        .unwrap_or(0.0);
    println!(
        "{} t={:.0}s frames {} gaps {} errors {} fps {:.3} (requested {:.3}) latency mean {:.2} max {:.2} ms period err mean {:.1} max {:.1} ppm ({} pairs) | rss {} kB fds {} dmabuf fds {} maps {} sys dmabufs {:?} | restarts {} reopens {} changes {} | stream {:?} fallback {}",
        stamp(),
        t0.elapsed().as_secs_f64(),
        w.frames,
        w.gaps,
        w.errors,
        fps,
        requested,
        w.latency_sum / w.frames.max(1) as f64,
        w.latency_max,
        w.period_err_sum / w.periods.max(1) as f64,
        w.period_err_max,
        w.periods,
        rss,
        fds,
        dmabufs,
        maps,
        system_dmabufs(),
        restarts,
        reopens,
        changes,
        stream.stats(),
        stream.frame_sync_fallback(),
    );
}
