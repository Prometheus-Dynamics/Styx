#[cfg(all(feature = "hooks", feature = "file-backend"))]
use std::num::NonZeroU32;
#[cfg(all(feature = "hooks", feature = "file-backend"))]
use std::time::Duration;
#[cfg(all(feature = "hooks", feature = "file-backend"))]
use std::{env, path::PathBuf};

#[cfg(all(feature = "hooks", feature = "file-backend"))]
use styx::DeviceIdentity;
#[cfg(all(feature = "hooks", feature = "file-backend"))]
use styx::prelude::*;

#[cfg(not(all(feature = "hooks", feature = "file-backend")))]
fn main() {
    eprintln!("Enable the `hooks` and `file-backend` features to run this example.");
}

#[cfg(all(feature = "hooks", feature = "file-backend"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = env::args()
        .nth(1)
        .unwrap_or_else(|| "recordings".to_string());
    let frames: usize = env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let fps: u32 = env::var("STYX_RECORD_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);

    let device = virtual_device();
    let mode = device.backends[0].descriptor.modes[0].clone();

    let recorder = FrameRecorder::new(
        PathBuf::from(&out_dir),
        RecordingOptions {
            prefix: "frame".into(),
            format: RecordingFormat::Png,
            ..RecordingOptions::default()
        },
    )?;

    let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(&device).mode(mode.id))
        .decode_enabled(false)
        .encode_enabled(false)
        .record_output(recorder)
        .start()?;

    let mut count = 0;
    while count < frames {
        match pipeline.next_blocking(Duration::from_millis(2)) {
            RecvOutcome::Data(_) => count += 1,
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => break,
        }
    }

    let recorder = pipeline.stop_with_recorder().expect("recorder");
    println!("recorded {} frames to {out_dir}", recorder.paths().len());

    let replay_device = make_file_device("record-replay", recorder.into_paths(), fps, false);
    let handle = CaptureRequest::new(&replay_device).start()?;
    let mut replayed = 0;
    while replayed < 5 {
        match handle.recv_blocking(Duration::from_millis(10)) {
            RecvOutcome::Data(frame) => {
                replayed += 1;
                println!(
                    "replay #{replayed} ts={} format={:?}",
                    frame.meta().timestamp,
                    frame.meta().format.code
                );
            }
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => break,
        }
    }
    handle.stop();

    Ok(())
}

#[cfg(all(feature = "hooks", feature = "file-backend"))]
fn virtual_device() -> ProbedDevice {
    let res = Resolution::new(640, 360).unwrap();
    let interval = Interval {
        numerator: NonZeroU32::new(1).unwrap(),
        denominator: NonZeroU32::new(30).unwrap(),
    };
    let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
    let mode = Mode {
        id: ModeId {
            format,
            interval: Some(interval),
        },
        format,
        intervals: vec![interval].into(),
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
            display: "virtual-record".into(),
            keys: vec!["virtual".into()],
        },
        backends: vec![backend],
    }
}
