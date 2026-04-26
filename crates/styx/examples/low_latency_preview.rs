use std::num::NonZeroU32;
use std::time::Duration;

use styx::DeviceIdentity;
use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A preview path should prefer freshness over completeness. A depth of 1
    // minimizes queued latency, and the small pool keeps the virtual path honest.
    StyxConfig::new()
        .capture_queue_depth(1)
        .capture_pool(2, 1 << 18, 2)
        .apply();

    let device = virtual_device();
    let mode = device.backends[0].descriptor.modes[0].clone();
    let request = CaptureRequest::new(&device).mode(mode.id.clone());
    let mut pipeline = MediaPipelineBuilder::new(request)
        // Preview does not need decode or encode on this RG24 source.
        .decode_enabled(false)
        .encode_enabled(false)
        .start()?;

    #[cfg(feature = "preview-window")]
    let mut preview = PreviewWindow::for_mode("styx low-latency preview", &mode).ok();

    let mut frames = 0;
    while frames < 60 {
        match pipeline.next_blocking(Duration::from_millis(2)) {
            RecvOutcome::Data(frame) => {
                frames += 1;
                let meta = frame.meta();
                println!(
                    "#{frames:03} ts={} fmt={:?} stride={}",
                    meta.timestamp,
                    meta.format.code,
                    frame.plane_strides().first().copied().unwrap_or_default()
                );
                #[cfg(feature = "preview-window")]
                if let Some(win) = preview.as_mut() {
                    let _ = win.show_if_open(&frame);
                }
            }
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => break,
        }
    }

    let report = pipeline.health_report();
    println!(
        "preview fps={:.1?} queue={}/{} drops={} backpressure={} copies={} latency_p50_ms={:.2?}",
        report.output_fps,
        report.capture_queue_depth,
        report.capture_queue_capacity,
        report.drop_count,
        report.capture_backpressure_count,
        report.copy_count,
        report.latency_p50_ms
    );

    pipeline.stop();
    Ok(())
}

fn virtual_device() -> ProbedDevice {
    let res = Resolution::new(640, 360).unwrap();
    let interval = Interval {
        numerator: NonZeroU32::new(1).unwrap(),
        denominator: NonZeroU32::new(30).unwrap(),
    };
    let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
    let mode = Mode {
        id: ModeId {
            format,
            interval: Some(interval),
        },
        format,
        intervals: vec![interval].into(),
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
            display: "virtual-low-latency-preview".into(),
            keys: vec!["virtual".into()],
        },
        backends: vec![backend],
    }
}
