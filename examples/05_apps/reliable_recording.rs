#[cfg(feature = "file-backend")]
use std::env;
#[cfg(feature = "file-backend")]
use std::path::PathBuf;
#[cfg(feature = "file-backend")]
use std::time::Duration;

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

    let device = virtual_device();
    let recorder = FrameRecorder::new(
        PathBuf::from(&out_dir),
        RecordingOptions {
            prefix: "frame".into(),
            format: RecordingFormat::Png,
            ..RecordingOptions::default()
        },
    )?;

    let request = device.capture_request();
    let mut pipeline = MediaPipelineBuilder::new(request)
        .config(
            StyxConfig::new()
                .capture_queue_depth(8)
                .capture_pool(4, 1 << 20, 8),
        )
        .raw_frames()
        .sink("recording", recorder)
        .start_with_policy(CaptureStartPolicy::resilient())?;

    for _ in pipeline
        .frames_blocking(Duration::from_millis(10))
        .take_frames(frames)
    {}

    let report = pipeline.health_report();
    let recorder = pipeline.stop_with_recorder().expect("recorder");
    println!(
        "recorded {} frames to {out_dir} queue={}/{} drops={} drop_reasons={:?} copies={} bytes_moved={} p50_ms={:.2?}",
        recorder.paths().len(),
        report.capture_queue_depth,
        report.capture_queue_capacity,
        report.drop_count,
        report.drop_reasons,
        report.copy_count,
        report.bytes_moved,
        report.latency_p50_ms
    );

    Ok(())
}

#[cfg(feature = "file-backend")]
fn virtual_device() -> ProbedDevice {
    make_virtual_rgb_device("virtual-reliable-recording", 640, 360, 30)
}
