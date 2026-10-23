//! Simulated cameras through the real stack: probed as native cameras, planned and started
//! with the software ISP and the 3A loop, their sensor's limits enforced, and served by a camera
//! service with the sensor's flip controls.

use std::time::Duration;

use styx::ipc::{CameraService, FrameClient, SERVICE_ROTATION};
use styx::prelude::*;
use styx::sim::{SimSource, SimSpec};

fn spec() -> SimSpec {
    let mut s = SimSpec::new("ov9782");
    s.source = SimSource::Chart { light: 1.0 };
    s
}

fn luma_mean(f: &FrameLease) -> f64 {
    let planes = f.planes();
    let y = planes[0].data();
    let fmt = f.meta().format;
    let (w, h) = (
        fmt.resolution.width.get() as usize,
        fmt.resolution.height.get() as usize,
    );
    let stride = f.layout_slice()[0].stride;
    let mut sum = 0u64;
    for row in (0..h).step_by(4) {
        sum += y[row * stride..row * stride + w]
            .iter()
            .step_by(4)
            .map(|&v| u64::from(v))
            .sum::<u64>();
    }
    sum as f64 / ((h / 4) * (w / 4)) as f64
}

/// A processed capture of a simulated OV9782: NV12 frames from the software ISP, AE bringing
/// the chart to a sensible brightness, the sensor's exposure limit at the frame rate enforced.
#[test]
fn a_simulated_sensor_runs_the_processed_pipeline() {
    let device = styx::sim::device(spec()).unwrap();
    assert!(
        device.identity.display.contains("simulated"),
        "{}",
        device.identity.display
    );
    let plan = styx::planner::plan_frames(&device, &Frames::nv12().size(640, 400).fps(30)).unwrap();
    let mut frames = plan.start().unwrap();
    let mut last = None;
    for _ in 0..45 {
        if let RecvOutcome::Data(f) = frames.next_frame(Duration::from_secs(3)) {
            last = Some(f);
        }
    }
    let f = last.expect("frames");
    assert_eq!(f.meta().format.code, FourCc::NV12);
    let native = f
        .meta()
        .native()
        .copied()
        .expect("sensor values with the frame");
    assert!(native.exposure_ns > 0 && native.analog_gain >= 1.0);
    let mean = luma_mean(&f);
    assert!((40.0..200.0).contains(&mean), "luma {mean}");
    // 100 ms on a 30 fps frame: held to the sensor's limit, reported as such.
    let applied = frames.set_exposure_us(100_000).unwrap();
    assert!(applied.clamped, "{applied:?}");
    assert!(
        matches!(applied.value, ControlValue::Uint(us) if us < 34_000),
        "{applied:?}"
    );
}

/// A camera service over a simulated camera: frames to a client, and the sensor's flips as
/// service controls (rotation restarts the capture).
#[test]
fn a_camera_service_serves_a_simulated_camera() {
    let device = styx::sim::device(spec()).unwrap();
    let path = std::env::temp_dir().join(format!("styx-sim-{}.sock", std::process::id()));
    let _service = CameraService::new(device)
        .keep_streaming()
        .serve(&path)
        .unwrap();
    let client = FrameClient::request(&path, &Frames::gray().size(640, 400).fps(30)).unwrap();
    let mut got = 0;
    for _ in 0..20 {
        if let RecvOutcome::Data(f) = client.recv(Duration::from_secs(3)) {
            assert_eq!(f.meta().format.code, FourCc::GREY);
            got += 1;
        }
    }
    assert!(got >= 15, "{got} frames");
    let applied = client
        .set_control(SERVICE_ROTATION, ControlValue::Int(180))
        .unwrap();
    assert!(applied.restarted, "{applied:?}");
    let mut after = 0;
    for _ in 0..10 {
        if let RecvOutcome::Data(_) = client.recv(Duration::from_secs(3)) {
            after += 1;
        }
    }
    assert!(after >= 5, "{after} frames after the rotation");
}
