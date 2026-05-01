use std::sync::Arc;
use std::time::{Duration, Instant};

use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let device = virtual_device();
    let mode = device.default_mode().ok_or("virtual device missing mode")?;
    let decoder = Arc::new(PassthroughDecoder::new(mode.format.code));
    let mut pipeline = MediaPipelineBuilder::new(device.capture_request())
        .config(
            StyxConfig::new()
                .capture_queue_depth(8)
                .capture_pool(4, 1 << 18, 8),
        )
        .decoder(decoder)
        .hook(|frame| frame.grayscale())
        .start()?;

    let started = Instant::now();
    let mut next_report = started + Duration::from_secs(1);
    let mut frames = 0u32;
    while started.elapsed() < Duration::from_secs(3) {
        match pipeline.next_blocking_result(Duration::from_millis(5))? {
            RecvOutcome::Data(frame) => {
                frames = frames.saturating_add(1);
                std::hint::black_box(frame.payload_bytes());
            }
            RecvOutcome::Empty => {}
            RecvOutcome::Closed => break,
        }

        if Instant::now() >= next_report {
            let report = pipeline.health_report();
            let last_transition = report.recent_residency_transitions.last().copied();
            let graph_copied_bytes = report
                .graph
                .as_ref()
                .map(|graph| graph.copied_bytes)
                .unwrap_or(0);
            let graph_pressure = report
                .graph
                .as_ref()
                .map(|graph| graph.pressure_events)
                .unwrap_or(0);
            if let Some(error) = report.recent_stage_errors.last() {
                eprintln!("last_pipeline_error={error}");
            }
            if let Some(error) = pipeline.last_stage_error() {
                eprintln!("last_stage_error={error}");
            }
            println!(
                "fps={:.1?} queue={}/{} drops={} drop_reasons={:?} backpressure={} copies={} bytes_moved={} graph_copied_bytes={} graph_pressure={} p50={:.2?}ms source_p50={:.2?}ms inflight={} buffers last_transition={:?}",
                report.output_fps,
                report.capture_queue_depth,
                report.capture_queue_capacity,
                report.drop_count,
                report.drop_reasons,
                report.capture_backpressure_count,
                report.copy_count,
                report.bytes_moved,
                graph_copied_bytes,
                graph_pressure,
                report.latency_p50_ms,
                report.source_latency_p50_ms,
                report.external_inflight_buffers,
                last_transition
            );
            next_report += Duration::from_secs(1);
        }
    }

    println!("processed_frames={frames}");
    pipeline.stop();
    Ok(())
}

fn virtual_device() -> ProbedDevice {
    make_virtual_rgb_device("virtual-health", 640, 360, 30)
}
