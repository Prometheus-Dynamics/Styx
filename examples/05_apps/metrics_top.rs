//! A live table of every open camera's metrics (`styx::metrics`): frame rate measured against
//! configured, frames and drops by cause, sensor-to-delivery latency, ISP and CPU time per
//! frame, 3A state, buffers held, and each consumer of a shared capture.
//!
//! ```text
//! metrics_top [--camera NAME] [--consumers N] [--slow MS] [--seconds S] [--interval MS]
//!             [--prometheus | --json]
//!     Opens every camera (or those whose name contains NAME) for NV12 frames, with N
//!     consumers sharing each capture; with --slow the last consumer takes a frame only every
//!     MS milliseconds (its drops show on its row).
//!     With --verify, each consumer also measures frame rate, sequence gaps and latency itself
//!     from the frames it receives, printed next to the metrics at the end.
//! metrics_top --overhead
//!     What recording the metrics costs the frame path per frame (no camera needed).
//! metrics_top --service [PATH] [--prometheus | --json] [--seconds S] [--interval MS]
//!     The metrics of a camera service running in another process (default socket:
//!     $STYX_SOCKET or /tmp/styx-camera.sock, as `camera_service serve` uses).
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use styx::ipc::FrameClient;
use styx::metrics::{CameraMetrics, ConsumerMetrics, MetricsSnapshot, Window};
use styx::prelude::*;

struct Options {
    camera: Option<String>,
    consumers: usize,
    slow: Option<Duration>,
    seconds: Option<f64>,
    interval: Duration,
    service: Option<String>,
    prometheus: bool,
    json: bool,
    verify: bool,
}

fn options() -> Result<Options, String> {
    let mut o = Options {
        camera: None,
        consumers: 1,
        slow: None,
        seconds: None,
        interval: Duration::from_secs(1),
        service: None,
        prometheus: false,
        json: false,
        verify: false,
    };
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--camera" => o.camera = Some(value()?),
            "--consumers" => o.consumers = value()?.parse().map_err(|_| "--consumers N")?,
            "--slow" => {
                o.slow = Some(Duration::from_millis(
                    value()?.parse().map_err(|_| "--slow MS")?,
                ))
            }
            "--seconds" => o.seconds = Some(value()?.parse().map_err(|_| "--seconds S")?),
            "--interval" => {
                o.interval = Duration::from_millis(value()?.parse().map_err(|_| "--interval MS")?)
            }
            "--service" => {
                let path = match args.peek() {
                    Some(p) if !p.starts_with("--") => args.next().unwrap_or_default(),
                    _ => std::env::var("STYX_SOCKET")
                        .unwrap_or_else(|_| "/tmp/styx-camera.sock".into()),
                };
                o.service = Some(path);
            }
            "--overhead" => {
                // Warm up, then the median of several runs.
                styx::metrics::frame_path_cost(100_000);
                let mut runs: Vec<_> = (0..7)
                    .map(|_| styx::metrics::frame_path_cost(1_000_000))
                    .collect();
                runs.sort();
                println!(
                    "metrics on the frame path: {:.0} ns per frame (median of 7 x 1M frames; min {:.0}, max {:.0})",
                    runs[3].as_nanos(),
                    runs[0].as_nanos(),
                    runs[6].as_nanos()
                );
                std::process::exit(0);
            }
            "--prometheus" => o.prometheus = true,
            "--json" => o.json = true,
            "--verify" => o.verify = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    o.consumers = o.consumers.max(1);
    Ok(o)
}

fn ms(v: Option<f64>) -> String {
    v.map_or("-".into(), |v| format!("{v:.2}"))
}

fn window(w: &Window) -> String {
    format!("{}/{}/{}", ms(w.p50_ms), ms(w.p95_ms), ms(w.max_ms))
}

/// CPU per frame since the previous table (the snapshot's own figure is since the start).
fn recent_cpu(c: &CameraMetrics, last: &mut HashMap<u64, (u64, u64)>) -> String {
    let now = (c.cpu.total_ns, c.frames.captured);
    let recent = last.insert(c.id, now).and_then(|(cpu, frames)| {
        let frames = now.1.checked_sub(frames).filter(|f| *f > 0)?;
        Some(now.0.saturating_sub(cpu) as f64 / 1e3 / frames as f64)
    });
    match (recent, c.cpu.per_frame_us) {
        (Some(us), _) | (None, Some(us)) if c.cpu.threads > 0 => format!("{us:.0}"),
        _ => "-".into(),
    }
}

fn consumer_row(c: &ConsumerMetrics) {
    println!(
        "    {:<36} received {:>7}  dropped {:>6}  held {:>2}  hold ms p50/p95/max {}",
        c.label,
        c.received,
        c.dropped,
        c.held,
        window(&c.hold)
    );
}

fn table(s: &MetricsSnapshot, last: &mut HashMap<u64, (u64, u64)>) {
    let p = &s.process;
    println!(
        "pid {}  cameras {}  service clients {}  cpu {:.1} s  rss {:.1} MiB  threads {}  dma-bufs {} ({:.1} MiB)",
        p.pid,
        p.cameras_open,
        p.service_clients,
        p.cpu_ns as f64 / 1e9,
        p.rss_bytes as f64 / 1048576.0,
        p.threads,
        p.dmabufs,
        p.dmabuf_bytes as f64 / 1048576.0
    );
    println!(
        "{:<14} {:<7} {:<15} {:>11} {:>17} {:>15} {:>20} {:>14} {:>6} {:<34} {:>5} {:>4}",
        "camera",
        "backend",
        "mode",
        "fps cfg/now",
        "frames cap/rx",
        "drop gap/q/c/i",
        "lat ms p50/p95/max",
        "isp ms p50/p95",
        "cpu us",
        "3A",
        "held",
        "rst"
    );
    for c in &s.cameras {
        let d = &c.drops;
        let aaa = c.aaa.as_ref().map_or("-".into(), |a| {
            format!(
                "{} {}us x{} {}K {}lx{}",
                a.ae_state.as_deref().map_or("raw", |s| &s[..4]),
                a.exposure_us.map_or("-".into(), |v| format!("{v:.0}")),
                a.analogue_gain.map_or("-".into(), |v| format!("{v:.2}")),
                a.colour_temperature_k
                    .map_or("-".into(), |v| format!("{v:.0}")),
                a.lux.map_or("-".into(), |v| format!("{v:.0}")),
                a.flicker_hz
                    .map_or(String::new(), |v| format!(" ~{v:.0}Hz"))
            ) + &a.af_state.as_ref().map_or(String::new(), |state| {
                format!(
                    " af {state} {}D{} scans {}",
                    a.lens_position_dioptres
                        .map_or("-".into(), |d| format!("{d:.2}")),
                    if a.lens_settled == Some(false) {
                        " moving"
                    } else {
                        ""
                    },
                    a.af_scans.unwrap_or(0)
                )
            })
        });
        let isp = c.isp.as_ref().map_or("-".into(), |i| {
            format!("{} {}/{}", i.kind, ms(i.isp.p50_ms), ms(i.isp.p95_ms))
        });
        println!(
            "{:<14} {:<7} {:<15} {:>11} {:>17} {:>15} {:>20} {:>14} {:>6} {:<34} {:>5} {:>4}",
            c.name.chars().take(14).collect::<String>(),
            c.backend,
            c.mode,
            format!(
                "{}/{}",
                c.fps.configured.map_or("-".into(), |v| format!("{v:.1}")),
                c.fps.measured.map_or("-".into(), |v| format!("{v:.1}"))
            ),
            format!("{}/{}", c.frames.captured, c.frames.received),
            format!(
                "{}/{}/{}/{}",
                d.sensor_sequence_gaps, d.queue_overflow, d.corrupted, d.isp_skipped
            ),
            window(&c.latency.sensor_to_delivery),
            isp,
            recent_cpu(c, last),
            aaa,
            c.buffers.held,
            c.restarts.reconnects
        );
        if let Some(err) = &c.restarts.last_error {
            println!("    last error: {err}");
        }
        for consumer in &c.consumers {
            consumer_row(consumer);
        }
    }
}

#[cfg(feature = "frame-socket")]
fn json(s: &MetricsSnapshot) -> String {
    s.to_json()
}

#[cfg(not(feature = "frame-socket"))]
fn json(_: &MetricsSnapshot) -> String {
    "JSON needs the frame-socket feature (styx's metrics-serde)".into()
}

static STOP: AtomicBool = AtomicBool::new(false);

/// What a consumer saw of each frame: sequence number, sensor timestamp, and the time from the
/// timestamp to receiving it (ns).
type Seen = Vec<(Option<u32>, u64, Option<u64>)>;

/// A consumer thread: its camera, its frames (kept open until the end) and what it saw.
type Consumer = std::thread::JoinHandle<(String, Frames, Seen)>;

fn sequence(meta: &FrameMeta) -> Option<u32> {
    match meta.backend.as_ref()? {
        BackendFrameMeta::Native(n) => Some(n.sequence),
        BackendFrameMeta::V4l2(v) => Some(v.sequence),
        BackendFrameMeta::Uvc(u) => Some(u.sequence),
        BackendFrameMeta::Libcamera(_) => None,
    }
}

/// Frame rate, sequence gaps and latency percentiles from what a consumer saw (the last
/// `window` frames for rate and latency, as the metrics' windows).
fn verify(name: &str, seen: &Seen, window: usize) {
    let gaps: u64 = seen
        .windows(2)
        .filter_map(|w| Some(u64::from(w[1].0?.checked_sub(w[0].0?)?.saturating_sub(1))))
        .sum();
    let tail = &seen[seen.len().saturating_sub(window)..];
    let fps = match (tail.first(), tail.last()) {
        (Some(a), Some(b)) if b.1 > a.1 => (tail.len() - 1) as f64 * 1e9 / (b.1 - a.1) as f64,
        _ => 0.0,
    };
    let mut lat: Vec<u64> = tail.iter().filter_map(|s| s.2).collect();
    lat.sort_unstable();
    let at = |q: f64| {
        lat.get(((lat.len().saturating_sub(1)) as f64 * q).round() as usize)
            .map_or("-".into(), |v| format!("{:.2}", *v as f64 / 1e6))
    };
    println!(
        "verify {name}: received {}  fps (last {}) {fps:.2}  sequence gaps seen {gaps}  sensor->receive ms p50/p95/max {}/{}/{}",
        seen.len(),
        tail.len(),
        at(0.5),
        at(0.95),
        at(1.0)
    );
}

/// Open each camera for NV12 frames with `consumers` consumers sharing its capture; the last
/// takes a frame only every `slow`.
fn open_cameras(o: &Options) -> Vec<Consumer> {
    let mut threads = Vec::new();
    for device in probe_all() {
        let name = device.identity.display.clone();
        if o.camera
            .as_ref()
            .is_some_and(|n| !name.contains(n.as_str()))
        {
            continue;
        }
        let reqs = vec![Frames::nv12(); o.consumers];
        let consumers = match styx::planner::plan_many(&device, &reqs).map(|p| p.start()) {
            Ok(Ok(consumers)) => consumers,
            Ok(Err(e)) => {
                eprintln!("{name}: did not start: {e}");
                continue;
            }
            Err(e) => {
                eprintln!("{name}: no plan: {e}");
                continue;
            }
        };
        let last = consumers.len() - 1;
        for (i, mut frames) in consumers.into_iter().enumerate() {
            let slow = o.slow.filter(|_| i == last && last > 0 || o.consumers == 1);
            let name = format!("{name} consumer {i}");
            threads.push(std::thread::spawn(move || {
                let mut seen = Seen::new();
                while !STOP.load(Ordering::Relaxed) {
                    match frames.next_frame(Duration::from_millis(500)) {
                        RecvOutcome::Data(frame) => {
                            let meta = frame.meta();
                            let latency = meta
                                .clock
                                .and_then(|c| c.now_ns())
                                .and_then(|now| now.checked_sub(meta.timestamp));
                            seen.push((sequence(meta), meta.timestamp, latency));
                            if let Some(slow) = slow {
                                std::thread::sleep(slow);
                            }
                            drop(frame);
                        }
                        RecvOutcome::Empty => {}
                        RecvOutcome::Closed => break,
                    }
                }
                (name, frames, seen)
            }));
        }
    }
    threads
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let o = options()?;
    let cameras = if o.service.is_none() {
        open_cameras(&o)
    } else {
        Vec::new()
    };
    let start = Instant::now();
    let mut last = HashMap::new();
    loop {
        std::thread::sleep(o.interval);
        match &o.service {
            Some(path) if o.prometheus => print!("{}", FrameClient::service_metrics_text(path)?),
            #[cfg(feature = "frame-socket")]
            Some(path) => {
                let m = FrameClient::service_metrics(path)?;
                if o.json {
                    println!("{}", m.to_json());
                } else {
                    println!(
                        "service: clients {}  sent {}  skipped {}  copied {}  restarts {}  revoked {}  rejected {}",
                        m.clients, m.sent, m.skipped, m.copied, m.restarts, m.revoked, m.rejected
                    );
                    for c in &m.client_metrics {
                        consumer_row(c);
                    }
                    table(&m.snapshot, &mut last);
                }
            }
            #[cfg(not(feature = "frame-socket"))]
            Some(path) => print!("{}", FrameClient::service_metrics_text(path)?),
            None => {
                let s = styx::metrics::snapshot();
                if o.prometheus {
                    print!("{}", s.prometheus_text());
                } else if o.json {
                    println!("{}", json(&s));
                } else {
                    table(&s, &mut last);
                }
            }
        }
        println!();
        if o.seconds
            .is_some_and(|s| start.elapsed().as_secs_f64() >= s)
        {
            break;
        }
    }
    STOP.store(true, Ordering::Relaxed);
    // The captures stay open (nobody receives) until the metrics are compared.
    let ended: Vec<_> = cameras.into_iter().filter_map(|c| c.join().ok()).collect();
    if o.verify {
        let s = styx::metrics::snapshot();
        for c in &s.cameras {
            let d = &c.drops;
            println!(
                "metrics {}: received {}  fps measured {}  drops gap/queue/corrupt/isp {}/{}/{}/{}  sensor->receive ms p50/p95/max {}",
                c.name,
                c.frames.received,
                c.fps.measured.map_or("-".into(), |v| format!("{v:.2}")),
                d.sensor_sequence_gaps,
                d.queue_overflow,
                d.corrupted,
                d.isp_skipped,
                window(&c.latency.sensor_to_receive)
            );
            for consumer in &c.consumers {
                consumer_row(consumer);
            }
        }
        for (name, _, seen) in &ended {
            verify(name, seen, styx::metrics::WINDOW);
        }
    }
    drop(ended);
    Ok(())
}
