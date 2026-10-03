//! Cost and latency of processed native frames through the Styx API: one consumer, two
//! consumers of one capture (NV12 at the sensor size and RGB at half size, both from one PiSP
//! pass), and consumers in another process through the camera service.
//!
//! ```sh
//! native_isp_bench single FPS FRAMES [--read]          # NV12 1280x800
//! native_isp_bench shared FPS FRAMES [--read]          # NV12 1280x800 + RG24 640x400
//! native_isp_bench pyramid FPS FRAMES [--read] [--software]  # luma + 2 pyramid levels
//! native_isp_bench serve SOCKET SECONDS                # camera service for other processes
//! native_isp_bench client SOCKET nv12|rgb FRAMES [--read]
//! ```
//!
//! `--read` reads every pixel of each frame (a CPU consumer); without it frames are only
//! received (a consumer handing them to hardware). Latency is the frame's capture timestamp
//! (frame start) to the consumer having it. CPU covers the whole process.

use std::time::{Duration, Instant};

use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;

fn now_ns() -> u64 {
    TimestampClock::Monotonic.now_ns().unwrap_or(0)
}

fn proc_cpu() -> Duration {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            let rest = s.rsplit_once(')')?.1.split_whitespace().collect::<Vec<_>>();
            let u: u64 = rest.get(11)?.parse().ok()?;
            let k: u64 = rest.get(12)?.parse().ok()?;
            Some(Duration::from_millis((u + k) * 10))
        })
        .unwrap_or_default()
}

fn status_kib(key: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

/// What one consumer saw.
#[derive(Default)]
struct Seen {
    name: String,
    latencies: Vec<f64>,
    first_seq: Option<u32>,
    last_seq: u32,
    first_ts: u64,
    last_ts: u64,
    format: String,
    residency: String,
    sum: u64,
}

impl Seen {
    fn add(&mut self, f: &FrameLease, read: bool) {
        let ts = f.meta().timestamp;
        self.latencies
            .push(now_ns().saturating_sub(ts) as f64 / 1e6);
        let seq = f.meta().sequence().unwrap_or(0);
        if self.first_seq.is_none() {
            self.first_seq = Some(seq);
            self.first_ts = ts;
        }
        (self.last_seq, self.last_ts) = (seq, ts);
        let fmt = f.meta().format;
        self.format = format!(
            "{} {}x{}",
            fmt.code, fmt.resolution.width, fmt.resolution.height
        );
        self.residency = format!(
            "{:?} ({})",
            f.residency(),
            f.external_backing_kind().unwrap_or("owned")
        );
        if read {
            for p in f.planes() {
                self.sum += p.data().iter().map(|&v| u64::from(v)).sum::<u64>();
            }
        }
    }

    fn report(&self) {
        let mut l = self.latencies.clone();
        l.sort_by(f64::total_cmp);
        let at = |q: f64| l.get(((l.len().max(1) - 1) as f64 * q) as usize).copied();
        // Frames from another process carry no sequence: count them.
        let frames = match self.last_seq.wrapping_sub(self.first_seq.unwrap_or(0)) {
            0 => self.latencies.len().saturating_sub(1) as u32,
            n => n,
        };
        let fps = f64::from(frames) / ((self.last_ts.saturating_sub(self.first_ts)) as f64 / 1e9);
        println!(
            "  {}: {} frames, {} at {fps:.3} fps ({} sequence steps), {}; latency median {:.2} ms, p95 {:.2} ms, max {:.2} ms",
            self.name,
            self.latencies.len(),
            self.format,
            frames,
            self.residency,
            at(0.5).unwrap_or(0.0),
            at(0.95).unwrap_or(0.0),
            at(1.0).unwrap_or(0.0)
        );
    }
}

fn consume(
    name: &str,
    frames: usize,
    read: bool,
    mut next: impl FnMut() -> Option<FrameLease>,
) -> Seen {
    let mut seen = Seen {
        name: name.into(),
        ..Default::default()
    };
    // Skip the start-up frames (AE converging, buffers being mapped).
    for _ in 0..10 {
        if next().is_none() {
            return seen;
        }
    }
    for _ in 0..frames {
        let Some(f) = next() else { break };
        seen.add(&f, read);
    }
    seen
}

/// Per thread: name, CPU (scheduler run time) and voluntary context switches.
fn threads() -> Vec<(u32, String, Duration, u64)> {
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    let mut out: Vec<_> = dir
        .flatten()
        .filter_map(|e| {
            let tid = e.file_name().to_str()?.parse().ok()?;
            let name = std::fs::read_to_string(e.path().join("comm")).ok()?;
            let ns: u64 = std::fs::read_to_string(e.path().join("schedstat"))
                .ok()?
                .split_whitespace()
                .next()?
                .parse()
                .ok()?;
            let status = std::fs::read_to_string(e.path().join("status")).unwrap_or_default();
            let waits = status
                .lines()
                .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            Some((
                tid,
                name.trim().to_string(),
                Duration::from_nanos(ns),
                waits,
            ))
        })
        .collect();
    out.sort_by_key(|t| t.0);
    out
}

fn thread_report(before: &[(u32, String, Duration, u64)], frames: usize) {
    let n = frames.max(1) as f64;
    for (tid, name, cpu, waits) in threads() {
        let b = before.iter().find(|b| b.0 == tid);
        let cpu = cpu.saturating_sub(b.map_or(Duration::ZERO, |b| b.2));
        let waits = waits - b.map_or(0, |b| b.3);
        if cpu.is_zero() && waits == 0 {
            continue;
        }
        println!(
            "  thread {name}: {:.3} ms CPU per frame, {:.2} wake-ups per frame",
            cpu.as_secs_f64() * 1e3 / n,
            waits as f64 / n
        );
    }
}

fn summary(t0: Instant, cpu0: Duration, frames: usize) {
    let wall = t0.elapsed().as_secs_f64();
    let cpu = proc_cpu().saturating_sub(cpu0).as_secs_f64();
    println!(
        "  process: CPU {:.2} ms per frame ({:.1}% of a core over {wall:.1} s), peak RSS {:.1} MiB, RSS now {:.1} MiB",
        cpu * 1e3 / frames.max(1) as f64,
        100.0 * cpu / wall,
        status_kib("VmHWM:") as f64 / 1024.0,
        status_kib("VmRSS:") as f64 / 1024.0
    );
}

fn native_device() -> Result<ProbedDevice, String> {
    let probe = styx::probe_all_with_errors_with_config(&StyxConfig::default());
    probe
        .devices
        .into_iter()
        .find(|d| d.backends.iter().any(|b| b.kind == BackendKind::Native))
        .ok_or_else(|| "no native camera".into())
}

/// Exactly `fps`, at most 1280x800, up to three frames queued.
fn req(code: FourCc, fps: u32) -> FrameRequest {
    Frames::formats([code])
        .fps(fps)
        .size_at_most(1280, 800)
        .every_frame(3)
}

fn run(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| args.get(i).cloned().unwrap_or_default();
    let num = |i: usize, d: u64| arg(i).parse().unwrap_or(d);
    let read = args.iter().any(|a| a == "--read");
    let wait = Duration::from_secs(3);
    match arg(1).as_str() {
        "single" => {
            let (fps, frames) = (num(2, 30) as u32, num(3, 300) as usize);
            let dev = native_device()?;
            let plan = styx::planner::plan_many(&dev, &[req(FourCc::NV12, fps)])
                .map_err(|e| e.to_string())?;
            print!("{plan}");
            let mut out = plan.start().map_err(|e| e.to_string())?;
            let mut p = out.remove(0);
            let (t0, cpu0, th0) = (Instant::now(), proc_cpu(), threads());
            let seen = consume("nv12", frames, read, || match p.next_frame(wait) {
                RecvOutcome::Data(f) => Some(f),
                _ => None,
            });
            seen.report();
            summary(t0, cpu0, frames + 10);
            thread_report(&th0, frames + 10);
            p.stop();
        }
        "shared" => {
            let (fps, frames) = (num(2, 30) as u32, num(3, 300) as usize);
            let dev = native_device()?;
            let small = req(FourCc::RG24, fps).size(640, 400);
            let plan = styx::planner::plan_many(&dev, &[req(FourCc::NV12, fps), small])
                .map_err(|e| e.to_string())?;
            print!("{plan}");
            let mut out = plan.start().map_err(|e| e.to_string())?;
            let mut b = out.pop().ok_or("no consumer")?;
            let mut a = out.pop().ok_or("no consumer")?;
            let (t0, cpu0, th0) = (Instant::now(), proc_cpu(), threads());
            let other = std::thread::spawn(move || {
                let s = consume("rgb half", frames, read, || match b.next_frame(wait) {
                    RecvOutcome::Data(f) => Some(f),
                    _ => None,
                });
                b.stop();
                s
            });
            let seen = consume("nv12", frames, read, || match a.next_frame(wait) {
                RecvOutcome::Data(f) => Some(f),
                _ => None,
            });
            let other = other.join().map_err(|_| "consumer panicked")?;
            seen.report();
            other.report();
            summary(t0, cpu0, frames + 10);
            thread_report(&th0, frames + 10);
            a.stop();
        }
        "pyramid" => {
            // Level 1 from the PiSP's second output, level 2 box-filtered from it (or both
            // box-filtered on the CPU with --software).
            let (fps, frames) = (num(2, 30) as u32, num(3, 300) as usize);
            let source = if args.iter().any(|a| a == "--software") {
                PyramidSource::Software
            } else {
                PyramidSource::PreferHardware
            };
            let levels = 2u8;
            // Luma (NV12's Y plane): the planner attaches pyramid levels to luma frames.
            let r = Frames::gray()
                .fps(fps)
                .size_at_most(1280, 800)
                .every_frame(3)
                .pyramid(levels)
                .pyramid_source(source);
            let dev = native_device()?;
            let plan = styx::planner::plan_many(&dev, &[r]).map_err(|e| e.to_string())?;
            print!("{plan}");
            let mut out = plan.start().map_err(|e| e.to_string())?;
            let mut p = out.remove(0);
            let (t0, cpu0, th0) = (Instant::now(), proc_cpu(), threads());
            let mut level_seen = vec![String::new(); levels as usize];
            let mut missing = 0usize;
            let seen = consume("luma+pyramid", frames, read, || match p.next_frame(wait) {
                RecvOutcome::Data(f) => {
                    for (i, s) in level_seen.iter_mut().enumerate() {
                        match f.pyramid_level(i as u8 + 1) {
                            Some(l) => {
                                let fmt = l.meta().format;
                                *s = format!(
                                    "{} {}x{} ({})",
                                    fmt.code,
                                    fmt.resolution.width,
                                    fmt.resolution.height,
                                    l.external_backing_kind().unwrap_or("owned")
                                );
                                if read {
                                    let sum: u64 =
                                        l.planes()[0].data().iter().map(|&v| u64::from(v)).sum();
                                    std::hint::black_box(sum);
                                }
                            }
                            None => missing += 1,
                        }
                    }
                    Some(f)
                }
                _ => None,
            });
            seen.report();
            for (i, s) in level_seen.iter().enumerate() {
                println!("  pyramid level {}: {s}", i + 1);
            }
            println!("  frames missing a level: {missing}");
            summary(t0, cpu0, frames + 10);
            thread_report(&th0, frames + 10);
            p.stop();
        }
        "serve" => {
            let (path, secs) = (arg(2), num(3, 30));
            let dev = native_device()?;
            let service = CameraService::new(dev)
                .serve(&path)
                .map_err(|e| e.to_string())?;
            let (t0, cpu0) = (Instant::now(), proc_cpu());
            let mut planned = false;
            while t0.elapsed() < Duration::from_secs(secs) {
                std::thread::sleep(Duration::from_millis(500));
                if !planned && let Some(p) = service.plan() {
                    println!("{p}");
                    planned = true;
                }
            }
            let frames = (secs * 30) as usize;
            println!("  service stats: {:?}", service.stats());
            summary(t0, cpu0, frames);
            service.stop();
        }
        "client" => {
            let (path, which, frames) = (arg(2), arg(3), num(4, 300) as usize);
            let r = match which.as_str() {
                "rgb" => req(FourCc::RG24, 30).size(640, 400),
                _ => req(FourCc::NV12, 30),
            };
            let client = FrameClient::request(&path, &r).map_err(|e| e.to_string())?;
            let (t0, cpu0) = (Instant::now(), proc_cpu());
            let seen = consume(&which, frames, read, || match client.recv(wait) {
                RecvOutcome::Data(f) => Some(f),
                _ => None,
            });
            seen.report();
            summary(t0, cpu0, frames);
        }
        _ => return Err("usage: native_isp_bench single|shared|pyramid|serve|client ...".into()),
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Err(e) = run(&args) {
        eprintln!("native_isp_bench: {e}");
        std::process::exit(1);
    }
}
