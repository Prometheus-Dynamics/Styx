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

/// The raw frames of a simulated camera's processed capture: a low-priority raw client (a
/// recorder) next to a vision client, the service keeping the raw companions on, gets 16-bit
/// Bayer frames of the sensor mode, named as `delivered()` says.
#[test]
fn a_raw_client_records_a_simulated_camera() {
    let device = styx::sim::device(spec()).unwrap();
    let path = std::env::temp_dir().join(format!("styx-sim-raw-{}.sock", std::process::id()));
    let config = styx::capture_api::StyxConfig::default().native_sensor_raw(true);
    let _service = CameraService::new(device)
        .keep_streaming()
        .capture_config(config)
        .serve(&path)
        .unwrap();
    // A vision client at the sensor's full size, as HeliOS runs (a low-priority client never
    // changes the capture, so a vision client on a smaller mode leaves no raw frames of it).
    let vision = FrameClient::request(&path, &Frames::gray().size(1280, 800).fps(30)).unwrap();
    assert!(matches!(
        vision.recv(Duration::from_secs(3)),
        RecvOutcome::Data(_)
    ));
    let raw = FrameClient::options(&path)
        .low_priority()
        .request(&Frames::sensor_raw().every_frame(4))
        .unwrap();
    let delivered = raw.delivered().expect("delivered");
    assert_eq!(delivered.format, FourCc::new(*b"BYR2"));
    let mut got = 0;
    for _ in 0..10 {
        let _ = vision.recv(Duration::from_millis(10));
        if let RecvOutcome::Data(f) = raw.recv(Duration::from_secs(3)) {
            let fmt = f.meta().format;
            assert_eq!(fmt.code, FourCc::new(*b"BYR2"));
            let size = (fmt.resolution.width.get(), fmt.resolution.height.get());
            assert_eq!(size, delivered.size);
            let bytes: usize = f.planes().iter().map(|p| p.data().len()).sum();
            assert_eq!(bytes, (size.0 * size.1 * 2) as usize);
            got += 1;
        }
    }
    assert!(got >= 5, "{got} raw frames");
}

/// `scene=gltf`: a glTF scene through the lens, frames at the sensor's exposure, and each frame's
/// ground truth (the camera's pose, the `truth:` points in the delivered frame) by sequence.
#[test]
fn a_gltf_scene_writes_each_frames_truth() {
    let truth = std::env::temp_dir().join(format!("styx-sim-truth-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&truth);
    let device = styx::sim::device_from(&format!(
        "ov9782,scene=gltf,path={}/tests/data/sim_scene.gltf,truth={}",
        env!("CARGO_MANIFEST_DIR"),
        truth.display()
    ))
    .unwrap();
    let plan =
        styx::planner::plan_frames(&device, &Frames::gray().size(1280, 800).fps(30)).unwrap();
    let mut frames = plan.start().unwrap();
    let mut sequences = Vec::new();
    for _ in 0..10 {
        if let RecvOutcome::Data(f) = frames.next_frame(Duration::from_secs(3)) {
            sequences.push(f.meta().native().map(|n| n.sequence).unwrap());
        }
    }
    drop(frames);
    assert!(sequences.len() >= 5, "{sequences:?}");
    let text = std::fs::read_to_string(&truth).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() >= sequences.len(), "{} lines", lines.len());
    // Every delivered frame has its truth: the square's four corners, not the hidden point.
    for seq in &sequences {
        let line = lines
            .iter()
            .find(|l| l.starts_with(&format!("{{\"sequence\":{seq},")))
            .unwrap_or_else(|| panic!("no truth for frame {seq}"));
        assert_eq!(line.matches("\"name\":\"square/").count(), 4, "{line}");
        assert!(!line.contains("\"hidden\""), "{line}");
        assert!(
            line.contains("\"position\":[0.000000,0.000000,0.000000]"),
            "{line}"
        );
    }
    let _ = std::fs::remove_file(truth);
}

/// Pyramid levels from the GPU (`PyramidSource::Gpu`): planned as GPU steps, attached to each
/// frame, and bit for bit the CPU's box filter of the frame.
#[cfg(feature = "gpu-wgpu")]
#[test]
fn gpu_pyramid_levels_are_the_cpus() {
    let device = styx::sim::device(spec()).unwrap();
    let req = Frames::gray()
        .size(1280, 800)
        .fps(30)
        .pyramid(3)
        .pyramid_source(PyramidSource::Gpu);
    let plan = styx::planner::plan_frames(&device, &req).unwrap();
    let gpu_steps = plan
        .steps
        .iter()
        .filter(|s| s.execution == styx::planner::StepExecution::Gpu)
        .count();
    assert_eq!(gpu_steps, 3, "{plan}");
    let mut frames = plan.start().unwrap();
    let mut checked = 0;
    for _ in 0..10 {
        let RecvOutcome::Data(f) = frames.next_frame(Duration::from_secs(3)) else {
            continue;
        };
        let mut above = None::<FrameLease>;
        for level in 1..=3u8 {
            let got = f.pyramid_level(level).expect("level attached");
            let want = box_downscale_luma(above.as_ref().unwrap_or(&f), 1).unwrap();
            let (g, w) = (got.luma_rows().unwrap(), want.luma_rows().unwrap());
            assert_eq!((g.row_bytes(), g.len()), (w.row_bytes(), w.len()));
            assert_eq!(g.row_bytes(), 1280 >> level);
            for y in 0..g.len() {
                assert_eq!(
                    g.row(y).unwrap(),
                    w.row(y).unwrap(),
                    "level {level} row {y}"
                );
            }
            above = Some(want);
        }
        checked += 1;
    }
    assert!(checked >= 5, "{checked} frames");
}

/// A capture error the capture recovers from stays in the metrics as history (its time, and
/// `recovered` once frames come again), not as the capture's error now.
#[test]
fn a_recovered_capture_error_is_history() {
    let mut s = spec();
    s.faults.fail_every = Some(Duration::from_millis(400));
    let device = styx::sim::device(s).unwrap();
    let plan = styx::planner::plan_frames(&device, &Frames::gray().size(640, 400).fps(30)).unwrap();
    let mut frames = plan.start().unwrap();
    let mut recovered = None;
    for _ in 0..200 {
        let _ = frames.next_frame(Duration::from_millis(100));
        let r = frames.metrics().restarts;
        if r.last_error.is_some() && r.recovered {
            recovered = Some(r);
            break;
        }
    }
    let r = recovered.unwrap_or_else(|| panic!("{:?}", frames.metrics().restarts));
    assert!(r.last_error_unix_ms.is_some(), "{r:?}");
}

/// Several regions on the software ISP: region 0 is the frame and the others its region
/// companions, each processed at full resolution in the frame's pass, in NV12, and moved
/// while running.
#[test]
fn the_software_isp_processes_every_region() {
    let device = styx::sim::device(spec()).unwrap();
    let (a, b) = (
        FrameRect::new(64, 64, 128, 128),
        FrameRect::new(640, 400, 256, 128),
    );
    let req = Frames::nv12().regions([a, b]).fps(30).skip_stale_regions();
    let plan = styx::planner::plan_frames(&device, &req).unwrap();
    assert_eq!(
        plan.delivered().regions,
        vec![
            Some(styx::planner::RoiCrop::Isp),
            Some(styx::planner::RoiCrop::IspPass)
        ],
        "{plan}"
    );
    let mut frames = plan.start().unwrap();
    let mut seen = None;
    for _ in 0..30 {
        if let RecvOutcome::Data(f) = frames.next_frame(Duration::from_secs(3)) {
            seen = Some(f);
        }
    }
    let f = seen.expect("frames");
    let size = |f: &FrameLease| {
        let r = f.meta().format.resolution;
        (r.width.get(), r.height.get())
    };
    assert_eq!((size(&f), f.meta().crop), ((128, 128), Some(a)));
    let region = f.region(1).expect("region 1");
    assert_eq!(region.meta().format.code, FourCc::NV12);
    assert_eq!((size(region), region.meta().crop), ((256, 128), Some(b)));
    assert_eq!(region.meta().timestamp, f.meta().timestamp);
    assert!((20.0..230.0).contains(&luma_mean(region)));

    let moved = FrameRect::new(320, 200, 64, 64);
    frames.roi().set_regions(&[a, moved]);
    let mut got = None;
    for _ in 0..30 {
        if let RecvOutcome::Data(f) = frames.next_frame(Duration::from_secs(3))
            && f.region(1).and_then(|r| r.meta().crop) == Some(moved)
        {
            got = Some(f);
            break;
        }
    }
    let f = got.expect("the moved region");
    assert_eq!(size(f.region(1).unwrap()), (64, 64));
}
