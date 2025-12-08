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
    let paths: Vec<PathBuf> = env::args().skip(1).map(PathBuf::from).collect();
    if paths.is_empty() {
        eprintln!(
            "usage: cargo run -p styx --features file-backend --example file_replay <file> [file2 ...]"
        );
        return Ok(());
    }
    let fps = env::var("STYX_FILE_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);

    let device = make_file_device("file-replay", paths, fps, true);
    let backend = &device.backends[0];

    let mut request = CaptureRequest::new(&device);
    if let Some(ctrl) = backend
        .descriptor
        .controls
        .iter()
        .find(|c| c.name == "file.image_fps")
    {
        request = request.control(ctrl.id, ControlValue::Uint(fps));
    }

    let mut handle = request.start()?;
    println!("available controls:");
    for ctrl in &backend.descriptor.controls {
        println!(
            "- {} ({:?}) access={:?} default={:?}",
            ctrl.name, ctrl.id, ctrl.access, ctrl.default
        );
    }

    #[cfg(feature = "preview-window")]
    let mut preview = {
        let fmt = backend.descriptor.modes[0].format;
        PreviewWindow::new(
            "file-replay",
            fmt.resolution.width.get(),
            fmt.resolution.height.get(),
        )
        .ok()
    };

    let mut frames = 0;
    while frames < 90 {
        match handle.recv_blocking(Duration::from_millis(10)) {
            RecvOutcome::Data(frame) => {
                frames += 1;
                let meta = frame.meta();
                println!(
                    "#{frames:03} ts={} mode={:?} stride={}",
                    meta.timestamp,
                    meta.format,
                    frame.plane_strides().get(0).copied().unwrap_or_default()
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

    handle.stop();
    Ok(())
}
