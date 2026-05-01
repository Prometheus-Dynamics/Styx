use std::time::{Duration, Instant};

use styx::prelude::*;

#[derive(Debug, Clone, Default)]
struct RunStats {
    frames: u64,
    elapsed: Duration,
    cpu_percent: f64,
    loop_ns: Vec<u64>,
    payload_bytes: u64,
    pipeline_copy_count: u64,
    pipeline_bytes_moved: u64,
    latency_p50_ms: Option<f64>,
    latency_p95_ms: Option<f64>,
    source_latency_p50_ms: Option<f64>,
    source_latency_p95_ms: Option<f64>,
    capture_queue_depth: u64,
    capture_queue_capacity: u64,
    external_inflight_buffers: u64,
    external_inflight_bytes: u64,
    zero_copy_frames: u64,
    copied_capture_frames: u64,
    host_owned_frames: u64,
    host_external_frames: u64,
    dmabuf_frames: u64,
    gpu_texture_frames: u64,
    compressed_frames: u64,
    graph_copied_bytes: u64,
    graph_duration_ns: u64,
    graph_node_total_ns: u64,
    graph_node_handler_ns: u64,
    graph_node_cpu_ns: u64,
    graph_edge_wait_ns: u64,
    graph_transport_apply_ns: u64,
    graph_adapter_ns: u64,
    graph_transport_bytes: u64,
    graph_transport_count: u64,
    graph_payload_clones: u64,
    graph_unique_handoffs: u64,
    graph_shared_handoffs: u64,
    graph_nodes_executed: u64,
    graph_pressure_events: u64,
}

impl RunStats {
    fn fps(&self) -> f64 {
        if self.elapsed.is_zero() {
            0.0
        } else {
            self.frames as f64 / self.elapsed.as_secs_f64()
        }
    }

    fn avg_loop_us(&self) -> f64 {
        if self.loop_ns.is_empty() {
            0.0
        } else {
            self.loop_ns.iter().sum::<u64>() as f64 / self.loop_ns.len() as f64 / 1_000.0
        }
    }

    fn p95_loop_us(&self) -> f64 {
        percentile(&self.loop_ns, 0.95).unwrap_or(0) as f64 / 1_000.0
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let frames = std::env::var("STYX_EXAMPLE_FRAMES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(180);
    let graph_metrics_level = graph_metrics_level_from_env();

    let selected = CameraRequest::new()
        .backend_priority([BackendKind::V4l2, BackendKind::Libcamera])
        .try_format_priority(["YUYV", "NV12", "MJPG", "JPEG", "RG24", "RGB3", "BGR3"])?
        .max_resolution(1920, 1080)
        .fastest_interval()
        .select()?;

    let mode = selected_mode(&selected).ok_or("selected mode not present in descriptor")?;
    println!(
        "camera={} backend={:?} format={} resolution={}x{} interval={}",
        selected.device.identity.display,
        selected.backend,
        mode.format.code,
        mode.format.resolution.width,
        mode.format.resolution.height,
        selected
            .interval
            .map(|interval| format!("{}/{}", interval.numerator, interval.denominator))
            .unwrap_or_else(|| "default".to_string())
    );
    println!("graph_metrics_level={graph_metrics_level:?}");

    let direct = run_capture_only(&selected, frames)?;
    let direct_decode = run_direct_decode(&selected, &mode, frames)?;
    let graph = run_graph_pipeline(&selected, &mode, frames, graph_metrics_level)?;

    print_section("capture_only", &direct);
    print_section("direct_capture_decode", &direct_decode);
    print_section("daedalus_graph_pipeline", &graph);
    print_overhead(&direct_decode, &graph);
    Ok(())
}

fn selected_mode(selected: &SelectedCamera) -> Option<Mode> {
    selected
        .device
        .backends
        .iter()
        .find(|backend| backend.kind == selected.backend)
        .and_then(|backend| {
            backend
                .descriptor
                .modes
                .iter()
                .find(|mode| mode.id == selected.mode)
                .cloned()
        })
}

fn run_capture_only(
    selected: &SelectedCamera,
    frames: u64,
) -> Result<RunStats, Box<dyn std::error::Error>> {
    let handle = selected.start_with_policy(CaptureStartPolicy::resilient())?;
    let started = Instant::now();
    let cpu_start = process_cpu_ticks();
    let mut stats = RunStats::default();
    while stats.frames < frames {
        let tick = Instant::now();
        match handle.recv_blocking(Duration::from_millis(250)) {
            RecvOutcome::Data(frame) => {
                stats.loop_ns.push(tick.elapsed().as_nanos() as u64);
                record_frame(&mut stats, &frame);
            }
            RecvOutcome::Empty => {}
            RecvOutcome::Closed => break,
        }
    }
    stats.elapsed = started.elapsed();
    stats.cpu_percent = cpu_percent(cpu_start, stats.elapsed);
    apply_health_report(&mut stats, handle.health_report());
    handle.stop();
    Ok(stats)
}

fn run_direct_decode(
    selected: &SelectedCamera,
    mode: &Mode,
    frames: u64,
) -> Result<RunStats, Box<dyn std::error::Error>> {
    let handle = selected.start_with_policy(CaptureStartPolicy::resilient())?;
    let decode = mode.decode_to_rg24();
    #[cfg(target_os = "linux")]
    let shared_pool = SharedBufferPool::with_limits(2, decode.shared_output_bytes, 4)?;
    let started = Instant::now();
    let cpu_start = process_cpu_ticks();
    let mut stats = RunStats::default();
    while stats.frames < frames {
        let tick = Instant::now();
        match handle.recv_blocking(Duration::from_millis(250)) {
            RecvOutcome::Data(frame) => {
                #[cfg(target_os = "linux")]
                let decoded = match decode.decoder.process_shared(&frame, &shared_pool)? {
                    Some(decoded) => decoded,
                    None => decode.decoder.process(frame)?,
                };
                #[cfg(not(target_os = "linux"))]
                let decoded = decode.decoder.process(frame)?;
                stats.loop_ns.push(tick.elapsed().as_nanos() as u64);
                record_frame(&mut stats, &decoded);
            }
            RecvOutcome::Empty => {}
            RecvOutcome::Closed => break,
        }
    }
    stats.elapsed = started.elapsed();
    stats.cpu_percent = cpu_percent(cpu_start, stats.elapsed);
    apply_health_report(&mut stats, handle.health_report());
    handle.stop();
    Ok(stats)
}

fn run_graph_pipeline(
    selected: &SelectedCamera,
    mode: &Mode,
    frames: u64,
    metrics_level: GraphMetricsLevel,
) -> Result<RunStats, Box<dyn std::error::Error>> {
    let decode = mode.decode_to_rg24();
    let mut pipeline = MediaPipelineBuilder::new(selected.capture_request())
        .decoder(decode.decoder)
        .without_encoder()
        .shared_decode_output(true)
        .owned_decode_fallback(false)
        .graph_metrics_level(metrics_level)
        .start_with_policy(CaptureStartPolicy::resilient())?;

    let started = Instant::now();
    let cpu_start = process_cpu_ticks();
    let mut stats = RunStats::default();
    while stats.frames < frames {
        let tick = Instant::now();
        match pipeline.next_blocking_result(Duration::from_millis(250))? {
            RecvOutcome::Data(frame) => {
                stats.loop_ns.push(tick.elapsed().as_nanos() as u64);
                record_frame(&mut stats, &frame);
                if let Some(graph) = pipeline.graph_telemetry_stats() {
                    stats.graph_copied_bytes =
                        stats.graph_copied_bytes.saturating_add(graph.copied_bytes);
                    stats.graph_duration_ns = stats
                        .graph_duration_ns
                        .saturating_add(graph.graph_duration_ns);
                    stats.graph_node_total_ns = stats
                        .graph_node_total_ns
                        .saturating_add(graph.node_total_duration_ns);
                    stats.graph_node_handler_ns = stats
                        .graph_node_handler_ns
                        .saturating_add(graph.node_handler_duration_ns);
                    stats.graph_node_cpu_ns = stats
                        .graph_node_cpu_ns
                        .saturating_add(graph.node_cpu_duration_ns);
                    stats.graph_edge_wait_ns = stats
                        .graph_edge_wait_ns
                        .saturating_add(graph.edge_wait_duration_ns);
                    stats.graph_transport_apply_ns = stats
                        .graph_transport_apply_ns
                        .saturating_add(graph.edge_transport_apply_duration_ns);
                    stats.graph_adapter_ns = stats
                        .graph_adapter_ns
                        .saturating_add(graph.edge_adapter_duration_ns);
                    stats.graph_transport_bytes = stats
                        .graph_transport_bytes
                        .saturating_add(graph.transport_bytes);
                    stats.graph_transport_count = stats
                        .graph_transport_count
                        .saturating_add(graph.transport_count);
                    stats.graph_payload_clones = stats
                        .graph_payload_clones
                        .saturating_add(graph.payload_clones);
                    stats.graph_unique_handoffs = stats
                        .graph_unique_handoffs
                        .saturating_add(graph.unique_handoffs);
                    stats.graph_shared_handoffs = stats
                        .graph_shared_handoffs
                        .saturating_add(graph.shared_handoffs);
                    stats.graph_nodes_executed = stats
                        .graph_nodes_executed
                        .saturating_add(graph.nodes_executed);
                    stats.graph_pressure_events = stats
                        .graph_pressure_events
                        .saturating_add(graph.pressure_events);
                }
            }
            RecvOutcome::Empty => {}
            RecvOutcome::Closed => break,
        }
    }
    stats.elapsed = started.elapsed();
    stats.cpu_percent = cpu_percent(cpu_start, stats.elapsed);
    apply_health_report(&mut stats, pipeline.health_report());
    pipeline.stop();
    Ok(stats)
}

fn graph_metrics_level_from_env() -> GraphMetricsLevel {
    match std::env::var("STYX_GRAPH_METRICS_LEVEL")
        .ok()
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("off") => GraphMetricsLevel::Off,
        Some("basic") => GraphMetricsLevel::Basic,
        Some("timing") => GraphMetricsLevel::Timing,
        Some("hardware") => GraphMetricsLevel::Hardware,
        Some("profile") => GraphMetricsLevel::Profile,
        Some("trace") => GraphMetricsLevel::Trace,
        _ => GraphMetricsLevel::Detailed,
    }
}

fn record_frame(stats: &mut RunStats, frame: &FrameLease) {
    stats.frames = stats.frames.saturating_add(1);
    stats.payload_bytes = stats
        .payload_bytes
        .saturating_add(frame.payload_bytes() as u64);
    match frame.meta().v4l2().map(|meta| meta.zero_copy) {
        Some(true) => stats.zero_copy_frames = stats.zero_copy_frames.saturating_add(1),
        Some(false) => {
            stats.copied_capture_frames = stats.copied_capture_frames.saturating_add(1);
        }
        None => {}
    }
    match frame.residency() {
        FrameResidency::HostOwned => {
            stats.host_owned_frames = stats.host_owned_frames.saturating_add(1)
        }
        FrameResidency::HostExternal => {
            stats.host_external_frames = stats.host_external_frames.saturating_add(1);
        }
        FrameResidency::Dmabuf => stats.dmabuf_frames = stats.dmabuf_frames.saturating_add(1),
        FrameResidency::GpuTexture => {
            stats.gpu_texture_frames = stats.gpu_texture_frames.saturating_add(1);
        }
        FrameResidency::CompressedPacket => {
            stats.compressed_frames = stats.compressed_frames.saturating_add(1);
        }
    }
}

fn print_section(name: &str, stats: &RunStats) {
    println!();
    println!("[{name}]");
    println!("frames={}", stats.frames);
    println!("elapsed_ms={:.3}", stats.elapsed.as_secs_f64() * 1_000.0);
    println!("cpu_percent={:.3}", stats.cpu_percent);
    println!("fps={:.3}", stats.fps());
    println!("latency_p50_ms={:.3?}", stats.latency_p50_ms);
    println!("latency_p95_ms={:.3?}", stats.latency_p95_ms);
    println!("source_latency_p50_ms={:.3?}", stats.source_latency_p50_ms);
    println!("source_latency_p95_ms={:.3?}", stats.source_latency_p95_ms);
    println!("avg_loop_us={:.3}", stats.avg_loop_us());
    println!("p95_loop_us={:.3}", stats.p95_loop_us());
    println!("payload_mb={:.3}", stats.payload_bytes as f64 / 1_048_576.0);
    println!("pipeline_copy_count={}", stats.pipeline_copy_count);
    println!("pipeline_bytes_moved={}", stats.pipeline_bytes_moved);
    println!("capture_queue_depth={}", stats.capture_queue_depth);
    println!("capture_queue_capacity={}", stats.capture_queue_capacity);
    println!(
        "external_inflight_buffers={}",
        stats.external_inflight_buffers
    );
    println!("external_inflight_bytes={}", stats.external_inflight_bytes);
    println!("capture_zero_copy_frames={}", stats.zero_copy_frames);
    println!("capture_copied_frames={}", stats.copied_capture_frames);
    println!("host_owned_frames={}", stats.host_owned_frames);
    println!("host_external_frames={}", stats.host_external_frames);
    println!("dmabuf_frames={}", stats.dmabuf_frames);
    println!("gpu_texture_frames={}", stats.gpu_texture_frames);
    println!("compressed_frames={}", stats.compressed_frames);
    println!("graph_nodes_executed={}", stats.graph_nodes_executed);
    println!(
        "graph_duration_avg_ms={:.3}",
        avg_ns(stats.graph_duration_ns, stats.frames) / 1_000_000.0
    );
    println!(
        "graph_node_total_avg_ms={:.3}",
        avg_ns(stats.graph_node_total_ns, stats.frames) / 1_000_000.0
    );
    println!(
        "graph_node_handler_avg_ms={:.3}",
        avg_ns(stats.graph_node_handler_ns, stats.frames) / 1_000_000.0
    );
    println!(
        "graph_node_cpu_avg_ms={:.3}",
        avg_ns(stats.graph_node_cpu_ns, stats.frames) / 1_000_000.0
    );
    println!(
        "graph_edge_wait_avg_us={:.3}",
        avg_ns(stats.graph_edge_wait_ns, stats.frames) / 1_000.0
    );
    println!(
        "graph_transport_apply_avg_us={:.3}",
        avg_ns(stats.graph_transport_apply_ns, stats.frames) / 1_000.0
    );
    println!(
        "graph_adapter_avg_us={:.3}",
        avg_ns(stats.graph_adapter_ns, stats.frames) / 1_000.0
    );
    println!("graph_copied_bytes={}", stats.graph_copied_bytes);
    println!("graph_transport_bytes={}", stats.graph_transport_bytes);
    println!("graph_transport_count={}", stats.graph_transport_count);
    println!("graph_payload_clones={}", stats.graph_payload_clones);
    println!("graph_unique_handoffs={}", stats.graph_unique_handoffs);
    println!("graph_shared_handoffs={}", stats.graph_shared_handoffs);
    println!("graph_pressure_events={}", stats.graph_pressure_events);
}

fn print_overhead(direct: &RunStats, graph: &RunStats) {
    println!();
    println!("[estimated_overhead]");
    println!(
        "avg_loop_overhead_us={:.3}",
        graph.avg_loop_us() - direct.avg_loop_us()
    );
    println!(
        "p95_loop_overhead_us={:.3}",
        graph.p95_loop_us() - direct.p95_loop_us()
    );
    println!("fps_delta={:.3}", graph.fps() - direct.fps());
    println!("graph_copied_bytes={}", graph.graph_copied_bytes);
    println!("graph_payload_clones={}", graph.graph_payload_clones);
}

fn percentile(values: &[u64], q: f64) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted.get(index).copied()
}

fn avg_ns(total_ns: u64, frames: u64) -> f64 {
    if frames == 0 {
        0.0
    } else {
        total_ns as f64 / frames as f64
    }
}

fn apply_health_report(stats: &mut RunStats, report: HealthReport) {
    stats.pipeline_copy_count = report.copy_count;
    stats.pipeline_bytes_moved = report.bytes_moved;
    stats.latency_p50_ms = report.latency_p50_ms;
    stats.latency_p95_ms = report.latency_p95_ms;
    stats.source_latency_p50_ms = report.source_latency_p50_ms;
    stats.source_latency_p95_ms = report.source_latency_p95_ms;
    stats.capture_queue_depth = report.capture_queue_depth;
    stats.capture_queue_capacity = report.capture_queue_capacity;
    stats.external_inflight_buffers = report.external_inflight_buffers;
    stats.external_inflight_bytes = report.external_inflight_bytes;
}

#[cfg(target_os = "linux")]
fn process_cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    let fields = after_comm.split_whitespace().collect::<Vec<_>>();
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    Some(utime.saturating_add(stime))
}

#[cfg(not(target_os = "linux"))]
fn process_cpu_ticks() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_second() -> f64 {
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 { ticks as f64 } else { 100.0 }
}

#[cfg(not(target_os = "linux"))]
fn clock_ticks_per_second() -> f64 {
    100.0
}

fn cpu_percent(start_ticks: Option<u64>, elapsed: Duration) -> f64 {
    match (start_ticks, process_cpu_ticks()) {
        (Some(start), Some(end)) if !elapsed.is_zero() => {
            let cpu_seconds = end.saturating_sub(start) as f64 / clock_ticks_per_second();
            (cpu_seconds / elapsed.as_secs_f64()) * 100.0
        }
        _ => 0.0,
    }
}
