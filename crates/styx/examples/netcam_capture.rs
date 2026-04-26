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
        .expect("usage: cargo run -p styx --features netcam --example netcam_capture <url> [width height fps]");
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

    // Network sources need retry/backoff knobs up front. The slightly deeper
    // queue absorbs bursty arrival without immediately dropping to empty.
    StyxConfig::new()
        .netcam_timeouts(10)
        .netcam_backoff(500, 5_000)
        .capture_queue_depth(4)
        .apply();

    let device = make_netcam_device("netcam", &url, width, height, fps);
    let mode = device.backends[0]
        .descriptor
        .modes
        .first()
        .ok_or("netcam device missing modes")?
        .id
        .clone();
    let decoder = Arc::new(MjpegDecoder::new(FourCc::new(*b"RG24")));
    let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(&device).mode(mode))
        .decoder(decoder)
        .start()?;

    #[cfg(feature = "preview-window")]
    let mut preview = PreviewWindow::for_descriptor("netcam", &device.backends[0].descriptor).ok();

    let mut frames = 0;
    while frames < 120 {
        match pipeline.next_blocking(Duration::from_millis(8)) {
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
