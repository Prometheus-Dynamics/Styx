use smallvec::smallvec;
use styx_capture::{CaptureDescriptor, ModeId};

use super::*;
use crate::DeviceIdentity;
use crate::prelude::{BackendHandle, ProbedBackend};

fn mode(code: FourCc, width: u32, height: u32, fps: u32) -> Mode {
    let format = MediaFormat::with_default_color(code, Resolution::new(width, height).unwrap());
    Mode {
        id: ModeId {
            format,
            interval: None,
        },
        format,
        intervals: smallvec![Interval::from_fps(fps).unwrap()],
        interval_stepwise: None,
    }
}

fn device(kind: BackendKind, handle: BackendHandle, modes: Vec<Mode>) -> ProbedDevice {
    ProbedDevice {
        identity: DeviceIdentity {
            display: "test-camera".into(),
            keys: Vec::new(),
        },
        backends: vec![ProbedBackend {
            kind,
            handle,
            descriptor: CaptureDescriptor {
                modes,
                controls: Vec::new(),
            },
            properties: Vec::new(),
        }],
    }
}

fn registry() -> CodecRegistryHandle {
    CodecRegistry::with_enabled_codecs().unwrap().handle()
}

fn usb_camera() -> ProbedDevice {
    device(
        BackendKind::Virtual,
        BackendHandle::Virtual,
        vec![
            mode(FourCc::YUYV, 640, 480, 30),
            mode(FourCc::NV12, 1280, 720, 10),
            mode(FourCc::MJPG, 1280, 720, 30),
            mode(FourCc::MJPG, 1920, 1080, 30),
        ],
    )
}

#[test]
fn luma_view_beats_decoding_at_the_same_resolution() {
    let dev = device(
        BackendKind::Virtual,
        BackendHandle::Virtual,
        vec![
            mode(FourCc::NV12, 1280, 720, 30),
            mode(FourCc::MJPG, 1280, 720, 30),
        ],
    );
    let plan = plan_frames_with(&dev, &FrameRequirements::luma().pyramid(2), &registry()).unwrap();
    assert_eq!(plan.mode.format.code, FourCc::NV12);
    assert!(matches!(plan.route, Route::LumaView));
    let kinds: Vec<StepKind> = plan.steps.iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        vec![
            StepKind::Capture,
            StepKind::LumaView,
            StepKind::Pyramid { level: 1 },
            StepKind::Pyramid { level: 2 },
        ]
    );
    let text = plan.to_string();
    assert!(
        text.contains("luma view") && text.contains("box filter"),
        "{text}"
    );
}

#[test]
fn minimum_resolution_picks_the_smallest_satisfying_mode() {
    let req = FrameRequirements::luma()
        .min_resolution(1200, 700)
        .min_fps(25);
    let plan = plan_frames_with(&usb_camera(), &req, &registry());
    #[cfg(feature = "codec-turbojpeg")]
    {
        let plan = plan.unwrap();
        // NV12 720p only runs at 10 fps, so MJPEG 720p wins over 1080p.
        assert_eq!(plan.mode.format.code, FourCc::MJPG);
        assert_eq!(plan.mode.format.resolution.height.get(), 720);
        assert!(
            plan.rejected
                .iter()
                .any(|r| r.reason.contains("below 25 fps"))
        );
    }
    #[cfg(not(feature = "codec-turbojpeg"))]
    assert!(plan.is_err());
}

#[test]
fn hardware_required_rejects_cpu_paths() {
    let req = FrameRequirements::luma().overrides(PlanOverrides {
        hardware: HardwarePolicy::Required,
        ..Default::default()
    });
    let err = plan_frames_with(&usb_camera(), &req, &registry()).unwrap_err();
    let PlanError::NoCandidates { rejected } = err else {
        panic!("expected NoCandidates, got {err:?}");
    };
    assert!(rejected.iter().any(|r| r.reason.contains("hardware")));
}

#[test]
fn forbidding_the_only_decoder_leaves_raw_paths() {
    let req = FrameRequirements::luma().overrides(PlanOverrides {
        forbid: vec!["turbojpeg-luma".into()],
        ..Default::default()
    });
    let plan = plan_frames_with(&usb_camera(), &req, &registry()).unwrap();
    assert_ne!(plan.mode.format.code, FourCc::MJPG);
}

#[test]
fn multiple_consumers_are_explicitly_unsupported() {
    let reqs = [FrameRequirements::luma(), FrameRequirements::luma()];
    assert!(matches!(
        plan_many(&usb_camera(), &reqs),
        Err(PlanError::MultipleConsumersUnsupported)
    ));
}

#[test]
fn priority_sets_decode_threads_and_queue_depth() {
    let dev = device(
        BackendKind::Virtual,
        BackendHandle::Virtual,
        vec![mode(FourCc::NV12, 640, 480, 30)],
    );
    let latency = plan_frames_with(&dev, &FrameRequirements::luma(), &registry()).unwrap();
    let throughput = plan_frames_with(
        &dev,
        &FrameRequirements::luma().priority(Priority::Throughput),
        &registry(),
    )
    .unwrap();
    assert!(latency.queue_depth < throughput.queue_depth);
    assert_eq!(latency.decode_threads, 0);
    assert_eq!(throughput.decode_threads, 1);
}

#[cfg(feature = "libcamera")]
#[test]
fn raspberry_pi_isp_supplies_the_first_pyramid_level() {
    let dev = device(
        BackendKind::Libcamera,
        BackendHandle::Libcamera {
            id: "/base/axi/pcie@1000120000/rp1/i2c@88000/ov9782@60".into(),
        },
        vec![mode(FourCc::NV12, 1280, 800, 30)],
    );
    let plan = plan_frames_with(&dev, &FrameRequirements::luma().pyramid(2), &registry()).unwrap();
    assert_eq!(plan.isp_pyramid_level, Some(1));
    let pyramid: Vec<StepExecution> = plan
        .steps
        .iter()
        .filter(|s| matches!(s.kind, StepKind::Pyramid { .. }))
        .map(|s| s.execution)
        .collect();
    assert_eq!(pyramid, vec![StepExecution::Hardware, StepExecution::Cpu]);

    let hardware_only = FrameRequirements::luma()
        .pyramid(2)
        .pyramid_source(PyramidSource::HardwareOnly);
    assert!(plan_frames_with(&dev, &hardware_only, &registry()).is_err());
}
