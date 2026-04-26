use std::sync::Arc;
use std::time::{Duration, Instant};

use styx::DeviceIdentity;
use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    StyxConfig::new()
        .capture_queue_depth(8)
        .capture_pool(4, 1 << 18, 8)
        .apply();

    let device = virtual_device();
    let mode = device.backends[0].descriptor.modes[0].clone();
    let decoder = Arc::new(PassthroughDecoder::new(mode.format.code));
    let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(&device))
        .decoder(decoder)
        .hook(|img| img.grayscale())
        .start()?;

    let started = Instant::now();
    let mut next_report = started + Duration::from_secs(1);
    let mut frames = 0u32;
    while started.elapsed() < Duration::from_secs(3) {
        match pipeline.next_blocking(Duration::from_millis(5)) {
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
            println!(
                "fps={:.1?} queue={}/{} drops={} backpressure={} copies={} p50={:.2?}ms source_p50={:.2?}ms inflight={} buffers last_transition={:?}",
                report.output_fps,
                report.capture_queue_depth,
                report.capture_queue_capacity,
                report.drop_count,
                report.capture_backpressure_count,
                report.copy_count,
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
    let res = Resolution::new(640, 360).unwrap();
    let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
    let mode = Mode {
        id: ModeId {
            format,
            interval: None,
        },
        format,
        intervals: Vec::new().into(),
        interval_stepwise: None,
    };
    let descriptor = CaptureDescriptor {
        modes: vec![mode.clone()],
        controls: Vec::new(),
    };
    let backend = ProbedBackend {
        kind: BackendKind::Virtual,
        handle: BackendHandle::Virtual,
        descriptor: descriptor.clone(),
        properties: vec![("kind".into(), "virtual".into())],
    };
    ProbedDevice {
        identity: DeviceIdentity {
            display: "virtual-health".into(),
            keys: vec!["virtual".into()],
        },
        backends: vec![backend],
    }
}
