//! A preview of an MJPEG camera served by the camera service, with the settings HeliOS uses:
//! NV12 at the preview size (the MJPEG frames decode to NV12 for it), the preview scales and
//! encodes them. JPEG frames must come out.

use std::path::PathBuf;
use std::time::Duration;

use styx::DeviceIdentity;
use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;
use styx::preview::{JpegBackend, Preview, PreviewConfig};

const FIXTURE: &[u8] = include_bytes!("../../../../testing/fixtures/c270_720p_rst.mjpeg");

/// The C270 fixture as a replayable MJPEG 1280x720 recording.
fn c270_recording(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("styx-{name}-{}.mcap", std::process::id()));
    let res = Resolution::new(1280, 720).unwrap();
    let format = MediaFormat::new(FourCc::MJPG, res, ColorSpace::Srgb);
    let header = RecordingHeader {
        device: DeviceIdentity {
            display: "c270 fixture".into(),
            keys: vec!["fixture:c270".into()],
        },
        backend: "v4l2".into(),
        format,
        interval: Interval::from_fps(30),
    };
    let mut recorder = StreamRecorder::with_header(&path, &header).unwrap();
    let starts: Vec<usize> = (0..FIXTURE.len() - 2)
        .filter(|&i| FIXTURE[i..i + 3] == [0xFF, 0xD8, 0xFF])
        .collect();
    for (k, &start) in starts.iter().enumerate() {
        let jpeg = &FIXTURE[start..*starts.get(k + 1).unwrap_or(&FIXTURE.len())];
        let mut buf = BufferPool::with_limits(1, jpeg.len(), 0).lease();
        buf.resize(jpeg.len());
        buf.as_mut_slice().copy_from_slice(jpeg);
        let meta = FrameMeta::new(format, k as u64 * 33_000_000);
        recorder
            .record(&FrameLease::single_plane(meta, buf, jpeg.len(), jpeg.len()))
            .unwrap();
    }
    recorder.finish().unwrap();
    path
}

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
