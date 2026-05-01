#[cfg(feature = "netcam")]
use std::env;
#[cfg(feature = "netcam")]
use std::sync::Arc;
#[cfg(feature = "netcam")]
use std::time::Duration;

#[cfg(feature = "netcam")]
use styx::prelude::*;

#[cfg(not(feature = "netcam"))]
fn main() {
    eprintln!("Enable the `netcam` feature to run this example.");
}

#[cfg(feature = "netcam")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = env::args()
        .nth(1)
        .expect("usage: cargo run -p styx-examples --features netcam,codec-jpeg-decoder --bin netcam_capture <url> [width height fps]");
    let width = env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(640);
    let height = env::args()
        .nth(3)
        .and_then(|v| v.parse().ok())
        .unwrap_or(480);
    let fps = env::args()
        .nth(4)
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);

    let device = make_netcam_device("netcam", &url, width, height, fps);
    let decoder = Arc::new(MjpegDecoder::new(FourCc::RG24));
    let mut pipeline = MediaPipelineBuilder::new(device.capture_request())
        .config(StyxConfig::netcam_preview())
        .decoder(decoder)
        .start()?;

    #[cfg(feature = "preview-window")]
    let mut preview =
        PreviewWindow::for_descriptor("netcam", device.default_descriptor().unwrap()).ok();

    let mut frames = 0;
    while frames < 120 {
        match pipeline.next_blocking_result(Duration::from_millis(8))? {
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

    pipeline.stop();
    Ok(())
}
