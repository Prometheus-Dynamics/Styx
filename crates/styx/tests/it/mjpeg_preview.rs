//! A preview of an MJPEG camera served by the camera service, with the settings HeliOS uses:
//! NV12 at the preview size (the MJPEG frames decode to NV12 for it), the preview scales and
//! encodes them. JPEG frames must come out.

use std::time::Duration;

use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;

use crate::support::c270::c270_recording;
use styx::preview::{JpegBackend, Preview, PreviewConfig};

#[test]
fn a_preview_of_an_mjpeg_camera_streams_jpeg_frames() {
    if JpegBackend::available().is_empty() {
        return;
    }
    let recording = c270_recording("mjpeg-preview");
    let device = CaptureRequest::replay_source(
        ReplaySourceConfig::new(&recording)
            .pacing(ReplayPacing::Realtime)
            .loop_forever(true),
    )
    .unwrap()
    .into_device();
    let path = std::env::temp_dir().join(format!("styx-mjpeg-preview-{}.sock", std::process::id()));
    let _service = CameraService::new(device).serve(&path).unwrap();

    let preview = Preview::from_service(
        FrameClient::options(&path),
        PreviewConfig::new()
            .size(640, 400)
            .max_fps(15.0)
            .quality(70),
    )
    .unwrap();
    let mut viewer = preview.subscribe();
    let frame = viewer
        .recv(Duration::from_secs(10))
        .unwrap_or_else(|| panic!("no preview frame: {:?}", preview.metrics()));
    assert_eq!(&frame.jpeg[..2], &[0xFF, 0xD8], "a JPEG");
    // The 16:9 source fits the 640x400 box at its aspect ratio.
    assert_eq!((frame.width, frame.height), (640, 360));
}
