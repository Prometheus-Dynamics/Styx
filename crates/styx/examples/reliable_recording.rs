#[cfg(feature = "file-backend")]
use std::env;
#[cfg(feature = "file-backend")]
use std::num::NonZeroU32;
#[cfg(feature = "file-backend")]
use std::path::PathBuf;
#[cfg(feature = "file-backend")]
use std::time::Duration;

#[cfg(feature = "file-backend")]
use styx::DeviceIdentity;
#[cfg(feature = "file-backend")]
use styx::prelude::*;

#[cfg(not(feature = "file-backend"))]
fn main() {
    eprintln!("Enable the `file-backend` feature to run this example.");
}

#[cfg(feature = "file-backend")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = env::args()
        .nth(1)
        .unwrap_or_else(|| "recordings".to_string());
    let frames: usize = env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);

    // Recording prefers completeness over minimum latency. Give capture a
    // deeper queue and enough spare pooled storage to absorb short stalls.
    StyxConfig::new()
        .capture_queue_depth(8)
        .capture_pool(4, 1 << 20, 8)
        .apply();

    let device = virtual_device();
    let mode = device.backends[0].descriptor.modes[0].clone();
    let recorder = FrameRecorder::new(
        PathBuf::from(&out_dir),
        RecordingOptions {
            prefix: "frame".into(),
            format: RecordingFormat::Png,
            ..RecordingOptions::default()
        },
    )?;

    let request = CaptureRequest::new(&device).mode(mode.id.clone());
    let mut pipeline = MediaPipelineBuilder::new(request)
        // The source is already RG24, so recording can skip decode/encode.
        .decode_enabled(false)
        .encode_enabled(false)
        .record_output(recorder)
        .start_with_policy(CaptureStartPolicy::resilient())?;

    let mut recorded = 0;
    while recorded < frames {
        match pipeline.next_blocking(Duration::from_millis(10)) {
            RecvOutcome::Data(_) => recorded += 1,
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => break,
        }
    }

    let report = pipeline.health_report();
    let recorder = pipeline.stop_with_recorder().expect("recorder");
    println!(
        "recorded {} frames to {out_dir} queue={}/{} drops={} copies={} p50_ms={:.2?}",
        recorder.paths().len(),
        report.capture_queue_depth,
        report.capture_queue_capacity,
        report.drop_count,
        report.copy_count,
        report.latency_p50_ms
    );

    Ok(())
}

#[cfg(feature = "file-backend")]
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
            display: "virtual-reliable-recording".into(),
            keys: vec!["virtual".into()],
        },
        backends: vec![backend],
    }
}
