//! The C270 MJPEG fixture as a replayable recording.

use std::path::PathBuf;

use styx::DeviceIdentity;
use styx::prelude::*;

const FIXTURE: &[u8] = include_bytes!("../../../../../testing/fixtures/c270_720p_rst.mjpeg");

/// The C270 fixture as a replayable MJPEG 1280x720 camera.
pub fn c270_recording(name: &str) -> PathBuf {
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
