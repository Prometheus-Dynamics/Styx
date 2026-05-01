use std::time::Duration;

use styx::prelude::*;

fn main() -> Result<(), CaptureError> {
    let device = virtual_device();
    let config = StyxConfig::new()
        .capture_queue_depth(8)
        .capture_pool(4, 1 << 18, 8);

    let handle = device.capture_request().config(config).start()?;
    println!(
        "virtual capture on {:?} at {:?} (interval {:?})",
        handle.backend(),
        handle.mode().format,
        handle.interval()
    );

    #[cfg(feature = "preview-window")]
    let mut preview = PreviewWindow::for_mode(
        "styx virtual",
        device.default_mode().ok_or(CaptureError::NoModes)?,
    )
    .ok();

    for (index, frame) in handle
        .frames_blocking(Duration::from_millis(2))
        .take_frames(12)
        .enumerate()
    {
        let frames = index + 1;
        let meta = frame.meta();
        let first = frame
            .planes()
            .first()
            .and_then(|p| p.data().first())
            .copied()
            .unwrap_or_default();
        println!(
            "#{frames:02} ts={} format={:?} first_byte={}",
            meta.timestamp, meta.format.code, first
        );
        #[cfg(feature = "preview-window")]
        if let Some(win) = preview.as_mut() {
            let _ = win.show_if_open(&frame);
        }
    }

    let metrics = handle.metrics();
    println!(
        "capture samples={} avg_wait_ms={:.2?} fps={:.1?}",
        metrics.samples(),
        metrics.avg_millis(),
        metrics.fps()
    );

    handle.stop();
    Ok(())
}

fn virtual_device() -> ProbedDevice {
    make_virtual_device(
        "virtual-rg24",
        [
            Mode::with_interval(
                MediaFormat::srgb(FourCc::RG24, 640, 360).unwrap(),
                Interval::from_fps(30).unwrap(),
            ),
            Mode::with_interval(
                MediaFormat::srgb(FourCc::RG24, 320, 180).unwrap(),
                Interval::from_fps(15).unwrap(),
            ),
        ],
    )
}
