use std::time::Duration;

#[cfg(feature = "preview-window")]
use styx::extras::preview_window::PreviewWindow;
use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let device = virtual_device();
    let mode = device.default_mode().ok_or("virtual device missing mode")?;
    let request = device.capture_request();
    let mut pipeline = MediaPipelineBuilder::new(request)
        .config(
            StyxConfig::new()
                .capture_queue_depth(1)
                .capture_pool(2, 1 << 18, 2),
        )
        .raw_frames()
        .start()?;

    #[cfg(feature = "preview-window")]
    let mut preview = PreviewWindow::for_mode("styx low-latency preview", mode).ok();

    for (index, frame) in pipeline
        .frames_blocking(Duration::from_millis(2))
        .take_frames(60)
        .enumerate()
    {
        let frames = index + 1;
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

    let report = pipeline.health_report();
    println!(
        "preview fps={:.1?} queue={}/{} drops={} drop_reasons={:?} backpressure={} copies={} bytes_moved={} latency_p50_ms={:.2?}",
        report.output_fps,
        report.capture_queue_depth,
        report.capture_queue_capacity,
        report.drop_count,
        report.drop_reasons,
        report.capture_backpressure_count,
        report.copy_count,
        report.bytes_moved,
        report.latency_p50_ms
    );

    pipeline.stop();
    Ok(())
}

fn virtual_device() -> ProbedDevice {
    make_virtual_rgb_device("virtual-low-latency-preview", 640, 360, 30)
}
