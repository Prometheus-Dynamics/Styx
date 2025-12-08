#[cfg(not(all(
    feature = "libcamera",
    feature = "codec-ffmpeg",
    feature = "preview-window"
)))]
fn main() {
    eprintln!(
        "Enable `libcamera`, `codec-ffmpeg`, and `preview-window` features to run this example."
    );
}

#[cfg(all(
    feature = "libcamera",
    feature = "codec-ffmpeg",
    feature = "preview-window"
))]
use styx::prelude::*;

#[cfg(all(
    feature = "libcamera",
    feature = "codec-ffmpeg",
    feature = "preview-window"
))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Arc;
    use std::time::Duration;

    let (device, mode) = pick_mjpeg_device().ok_or("no libcamera devices with MJPG mode found")?;
    let backend = &device.backends[0];
    println!(
        "using libcamera device '{}' mode {:?}",
        device.identity.display, mode.id.format
    );

    let decoder: Arc<dyn Codec> = Arc::new(FfmpegMjpegDecoder::new_rgb24()?);
    let encoder: Arc<dyn Codec> = Arc::new(FfmpegMjpegEncoder::new_rgb24()?);

    let handle = CaptureRequest::new(&device)
        .backend(backend.kind)
        .mode(mode.id.clone())
        .start()?;

    let mut preview = PreviewWindow::new(
        "libcamera + ffmpeg preview",
        mode.format.resolution.width.get(),
        mode.format.resolution.height.get(),
    )
    .ok();

    let mut frames: u64 = 0;
    loop {
        match handle.recv_blocking(Duration::from_millis(10)) {
            RecvOutcome::Data(frame) => {
                frames += 1;
                let decoded = match decoder.process(frame) {
                    Ok(f) => f,
                    Err(err) => {
                        eprintln!("decode failed: {err}");
                        continue;
                    }
                };
                let decoded_fmt = decoded.meta().format.code;
                if let Some(win) = preview.as_mut() {
                    if let Err(e) = win.show(&decoded) {
                        eprintln!("preview failed: {e}");
                    }
                }
                let encoded = match encoder.process(decoded) {
                    Ok(f) => f,
                    Err(err) => {
                        eprintln!("encode failed: {err}");
                        continue;
                    }
                };
                let encoded_fmt = encoded.meta().format.code;
                let bytes = encoded.planes().get(0).map(|p| p.data().len()).unwrap_or(0);
                println!(
                    "#{frames:06} src={:?} decoded={decoded_fmt:?} encoded={encoded_fmt:?} bytes={bytes}",
                    mode.id.format.code
                );
            }
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => break,
        }
    }

    handle.stop();
    Ok(())
}

#[cfg(all(
    feature = "libcamera",
    feature = "codec-ffmpeg",
    feature = "preview-window"
))]
fn pick_mjpeg_device() -> Option<(ProbedDevice, Mode)> {
    let mut best: Option<(u64, ProbedDevice, Mode)> = None;
    for mut device in probe_all() {
        let Some(backend) = device
            .backends
            .iter()
            .find(|b| b.kind == BackendKind::Libcamera)
        else {
            continue;
        };

        let Some(mode) = backend
            .descriptor
            .modes
            .iter()
            .filter(|m| m.id.format.code == FourCc::new(*b"MJPG"))
            .max_by_key(|m| {
                let res = m.id.format.resolution;
                u64::from(res.width.get()) * u64::from(res.height.get())
            })
            .cloned()
        else {
            continue;
        };

        let area = {
            let res = mode.id.format.resolution;
            u64::from(res.width.get()) * u64::from(res.height.get())
        };
        device.backends = vec![backend.clone()];
        match &mut best {
            None => best = Some((area, device.clone(), mode.clone())),
            Some((best_area, best_dev, best_mode)) => {
                if area > *best_area {
                    *best_area = area;
                    *best_dev = device.clone();
                    *best_mode = mode.clone();
                }
            }
        }
    }
    best.map(|(_, dev, mode)| (dev, mode))
}
