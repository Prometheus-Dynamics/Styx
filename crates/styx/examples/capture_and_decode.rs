use std::sync::Arc;
use std::time::Duration;

use styx::DeviceIdentity;
use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let device = virtual_device();
    let mode = device.backends[0].descriptor.modes[0].clone();

    let decoder = Arc::new(PassthroughDecoder::new(mode.format.code));

    let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(&device))
        .decoder(decoder)
        // Mutate the raw frame before decode.
        .frame_hook(|mut frame| {
            if let Some(mut plane) = frame.planes_mut().into_iter().next() {
                let stride = plane.stride().max(1);
                for (idx, byte) in plane.data().iter_mut().take(stride.min(32)).enumerate() {
                    *byte = byte.wrapping_add(idx as u8);
                }
            }
            frame
        })
        // Apply an image-level hook (requires `hooks` feature, enabled by default).
        .hook(|img| img.grayscale())
        .start()?;

    #[cfg(feature = "preview-window")]
    let mut preview = PreviewWindow::new(
        "styx capture+decode",
        mode.format.resolution.width.get(),
        mode.format.resolution.height.get(),
    )
    .ok();

    let mut frames = 0;
    while frames < 30 {
        match pipeline.next_blocking(Duration::from_millis(2)) {
            RecvOutcome::Data(frame) => {
                frames += 1;
                let meta = frame.meta();
                let first = frame
                    .planes()
                    .first()
                    .and_then(|p| p.data().first())
                    .copied()
                    .unwrap_or_default();
                println!(
                    "#{frames:02} ts={} fmt={:?} stride={} first_byte={}",
                    meta.timestamp,
                    meta.format.code,
                    frame.plane_strides().first().copied().unwrap_or_default(),
                    first
                );
                #[cfg(feature = "preview-window")]
                if let Some(win) = preview.as_mut() {
                    let _ = win.show(&frame);
                }
            }
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => break,
        }
    }

    let metrics = pipeline.metrics();
    println!(
        "capture avg_ms={:.2?} decode avg_ms={:.2?} processed={}",
        metrics.capture.avg_millis(),
        metrics.decode.avg_millis(),
        metrics.capture.samples()
    );

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
            display: "virtual-pipeline".into(),
            keys: vec!["virtual".into()],
        },
        backends: vec![backend],
    }
}
