use std::env;
#[cfg(target_os = "linux")]
use std::fs;
use std::time::{Duration, Instant};

use styx::prelude::*;

fn find_device() -> Result<ProbedDevice, Box<dyn std::error::Error>> {
    let selector = env::args().nth(1);
    let devices = probe_all();
    if devices.is_empty() {
        return Err("no devices found".into());
    }

    if let Some(selector) = selector {
        devices
            .into_iter()
            .find(|dev| {
                dev.identity.display.contains(&selector)
                    || dev.identity.keys.iter().any(|key| key.contains(&selector))
            })
            .ok_or_else(|| format!("no device matched selector `{selector}`").into())
    } else {
        devices
            .into_iter()
            .find(|dev| {
                dev.backends.iter().any(|backend| {
                    matches!(backend.kind, BackendKind::V4l2)
                        && !dev.identity.display.to_ascii_lowercase().contains("obs")
                })
            })
            .ok_or_else(|| "no non-virtual V4L2 device found".into())
    }
}

fn interval_fps(interval: Interval) -> f64 {
    interval.denominator.get() as f64 / interval.numerator.get() as f64
}

fn pick_fastest_interval(mode: &Mode) -> Option<Interval> {
    mode.intervals.iter().copied().max_by(|a, b| {
        let left = (a.denominator.get() as u64).saturating_mul(b.numerator.get() as u64);
        let right = (b.denominator.get() as u64).saturating_mul(a.numerator.get() as u64);
        left.cmp(&right)
    })
}

fn percentile(sorted: &[u64], q: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted.get(idx).copied()
}

#[cfg(target_os = "linux")]
fn process_cpu_ticks() -> Option<u64> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    let fields = after_comm.split_whitespace().collect::<Vec<_>>();
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    Some(utime.saturating_add(stime))
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_second() -> f64 {
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 { ticks as f64 } else { 100.0 }
}

#[cfg(not(target_os = "linux"))]
fn process_cpu_ticks() -> Option<u64> {
    None
}

#[cfg(not(target_os = "linux"))]
fn clock_ticks_per_second() -> f64 {
    100.0
}

fn measure_mode(device: &ProbedDevice, mode: &Mode) -> Result<(), Box<dyn std::error::Error>> {
    let interval = pick_fastest_interval(mode);
    let mut request = CaptureRequest::new(device)
        .backend(BackendKind::V4l2)
        .mode(mode.id.clone());
    if let Some(interval) = interval {
        request = request.interval(interval);
    }
    let mut pipeline = MediaPipelineBuilder::new(request).raw_frames().start()?;

    let start = Instant::now();
    let cpu_start = process_cpu_ticks();
    let mut frames = 0u32;
    let mut first_ts = None;
    let mut last_ts = None;
    let mut zero_copy_frames = 0u32;
    let mut copied_frames = 0u32;
    let mut frame_deltas_ns = Vec::new();
    let mut graph_copied_bytes = 0u64;

    while start.elapsed() < Duration::from_secs(3) {
        match pipeline.next_blocking_result(Duration::from_millis(100))? {
            RecvOutcome::Data(frame) => {
                frames += 1;
                if let Some(last) = last_ts.replace(frame.meta().timestamp) {
                    let delta = frame.meta().timestamp.saturating_sub(last);
                    if delta > 0 {
                        frame_deltas_ns.push(delta);
                    }
                } else {
                    first_ts.get_or_insert(frame.meta().timestamp);
                }
                match frame.meta().v4l2().map(|meta| meta.zero_copy) {
                    Some(true) => zero_copy_frames += 1,
                    Some(false) => copied_frames += 1,
                    None => {}
                }
                #[cfg(feature = "graph-pipeline")]
                if let Some(telemetry) = pipeline.graph_telemetry() {
                    graph_copied_bytes = graph_copied_bytes.saturating_add(
                        telemetry
                            .edge_metrics
                            .values()
                            .map(|metrics| metrics.copied_bytes)
                            .sum::<u64>(),
                    );
                }
            }
            RecvOutcome::Empty => {}
            RecvOutcome::Closed => break,
        }
    }

    let wall_secs = start.elapsed().as_secs_f64();
    let wall_fps = frames as f64 / wall_secs;
    let source_fps = match (first_ts, last_ts, frames) {
        (Some(first), Some(last), count) if count > 1 && last > first => {
            (count - 1) as f64 / ((last - first) as f64 / 1_000_000_000.0)
        }
        _ => 0.0,
    };
    frame_deltas_ns.sort_unstable();
    let median_delta_ms = percentile(&frame_deltas_ns, 0.5)
        .map(|ns| ns as f64 / 1_000_000.0)
        .unwrap_or(0.0);
    let p95_delta_ms = percentile(&frame_deltas_ns, 0.95)
        .map(|ns| ns as f64 / 1_000_000.0)
        .unwrap_or(0.0);
    let cpu_percent = match (cpu_start, process_cpu_ticks()) {
        (Some(start_ticks), Some(end_ticks)) => {
            let cpu_secs = end_ticks.saturating_sub(start_ticks) as f64 / clock_ticks_per_second();
            (cpu_secs / wall_secs) * 100.0
        }
        _ => 0.0,
    };
    let report = pipeline.health_report();
    let interval_desc = interval
        .map(|iv| {
            format!(
                "{}/{} ({:.2} fps target)",
                iv.numerator,
                iv.denominator,
                interval_fps(iv)
            )
        })
        .unwrap_or_else(|| "default".to_string());

    println!(
        "{:?} {}x{} interval={} wall_fps={:.2} source_fps={:.2} cpu_percent={:.1} median_delta_ms={:.2} p95_delta_ms={:.2} e2e_p50_ms={:.2?} e2e_p95_ms={:.2?} source_p50_ms={:.2?} source_p95_ms={:.2?} frames={} zero_copy={} copied={} pipeline_copies={} pipeline_bytes_moved={} graph_copied_bytes={} handoff={}",
        mode.format.code,
        mode.format.resolution.width,
        mode.format.resolution.height,
        interval_desc,
        wall_fps,
        source_fps,
        cpu_percent,
        median_delta_ms,
        p95_delta_ms,
        report.latency_p50_ms,
        report.latency_p95_ms,
        report.source_latency_p50_ms,
        report.source_latency_p95_ms,
        frames,
        zero_copy_frames,
        copied_frames,
        report.copy_count,
        report.bytes_moved,
        graph_copied_bytes,
        if frames == 0 {
            "no_frames"
        } else if zero_copy_frames == frames && graph_copied_bytes == 0 {
            "zero_copy_graph"
        } else if graph_copied_bytes == 0 {
            "graph_no_copy_capture_mixed"
        } else {
            "graph_copied"
        }
    );

    pipeline.stop();
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if !cfg!(feature = "v4l2") {
        println!("Enable the `v4l2` feature to run this example.");
        return Ok(());
    }
    if !cfg!(feature = "graph-pipeline") {
        println!("Enable the `graph-pipeline` feature to benchmark V4L2 through Daedalus.");
        return Ok(());
    }

    let device = find_device()?;
    let backend = device
        .backends
        .iter()
        .find(|backend| matches!(backend.kind, BackendKind::V4l2))
        .ok_or("device missing V4L2 backend")?;

    println!("device: {}", device.identity.display);
    println!("measuring V4L2 through the graph-backed pipeline for 3 seconds per mode");

    let mut modes = backend
        .descriptor
        .modes
        .iter()
        .filter(|mode| {
            let width = mode.format.resolution.width.get();
            let height = mode.format.resolution.height.get();
            (width == 1280 && height == 720)
                || (width == 1920 && height == 1080)
                || (width == 3840 && height == 2160)
        })
        .cloned()
        .collect::<Vec<_>>();

    modes.sort_by_key(|mode| {
        (
            mode.id.format.code.to_u32(),
            mode.id.format.resolution.width,
            mode.id.format.resolution.height,
        )
    });
    modes.dedup_by_key(|mode| {
        (
            mode.id.format.code.to_u32(),
            mode.id.format.resolution.width,
            mode.id.format.resolution.height,
        )
    });

    if modes.is_empty() {
        println!("no representative benchmark modes found on this device");
        return Ok(());
    }

    for mode in &modes {
        measure_mode(&device, mode)?;
    }

    Ok(())
}
