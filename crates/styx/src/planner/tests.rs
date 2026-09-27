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
fn consumers_share_a_mode_that_serves_all_of_them() {
    let camera = device(
        BackendKind::Virtual,
        BackendHandle::Virtual,
        vec![
            mode(FourCc::NV12, 640, 480, 30),
            mode(FourCc::NV12, 1280, 720, 30),
            mode(FourCc::NV12, 1920, 1080, 30),
        ],
    );
    // A small detector and a 720p consumer: the smallest mode covering both.
    let reqs = [
        FrameRequirements::luma().output_resolution(320, 180),
        FrameRequirements::luma().min_resolution(1280, 720),
    ];
    let plan = plan_many_with(&camera, &reqs, &registry()).unwrap();
    assert_eq!(plan.mode.format.resolution.width.get(), 1280);
    assert_eq!(plan.consumers.len(), 2);
    assert!(plan.consumers.iter().all(|c| c.mode.id == plan.mode.id));
    assert!(matches!(
        plan_many_with(&camera, &[], &registry()),
        Err(PlanError::NoConsumers)
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

#[test]
fn output_resolution_picks_the_smallest_covering_mode_and_scales_the_decode() {
    let dev = device(
        BackendKind::Virtual,
        BackendHandle::Virtual,
        vec![
            mode(FourCc::MJPG, 1920, 1080, 30),
            mode(FourCc::MJPG, 1280, 720, 30),
            mode(FourCc::MJPG, 160, 90, 30),
        ],
    );
    let req = FrameRequirements::luma().output_resolution(320, 180);
    let plan = plan_frames_with(&dev, &req, &registry());
    #[cfg(feature = "codec-turbojpeg")]
    {
        let plan = plan.unwrap();
        // 160x90 is too small; 1280x720 is the smallest mode covering 320x180, decoded at ¼.
        assert_eq!(plan.mode.format.resolution.width.get(), 1280);
        assert_eq!(plan.output_resolution(), (320, 180));
        let text = plan.to_string();
        assert!(text.contains("at 1/4 size (320x180)"), "{text}");

        let full = plan_frames_with(&dev, &FrameRequirements::luma(), &registry()).unwrap();
        assert!(plan.total.cpu_ms < full.total.cpu_ms * 0.5);

        // RGB through turbojpeg scales the same way.
        let rgb = FrameRequirements::formats([FourCc::RG24]).output_resolution(640, 360);
        let rgb = plan_frames_with(&dev, &rgb, &registry()).unwrap();
        assert_eq!(rgb.output_resolution(), (640, 360));
        // Even when a decoder that cannot scale (e.g. FFmpeg's) would otherwise be first.
        assert_eq!(rgb.decoder().unwrap().descriptor().impl_name, "turbojpeg");
    }
    #[cfg(not(feature = "codec-turbojpeg"))]
    assert!(plan.is_err());
}

#[test]
fn output_resolution_larger_than_every_mode_takes_the_largest() {
    let req = FrameRequirements::luma().output_resolution(4000, 3000);
    let plan = plan_frames_with(&usb_camera(), &req, &registry()).unwrap();
    // Without an MJPEG decoder the largest usable mode is NV12 720p.
    let largest = if cfg!(feature = "codec-turbojpeg") {
        (1920, 1080)
    } else {
        (1280, 720)
    };
    assert_eq!(plan.output_resolution(), largest);
}

#[cfg(feature = "libcamera")]
#[test]
fn raspberry_pi_isp_scales_to_the_output_resolution_keeping_the_field_of_view() {
    let dev = device(
        BackendKind::Libcamera,
        BackendHandle::Libcamera {
            id: "/base/axi/pcie@1000120000/rp1/i2c@88000/ov9782@60".into(),
        },
        // libcamera also lists smaller (ISP) sizes of other aspect ratios as modes.
        vec![
            mode(FourCc::NV12, 1280, 800, 30),
            mode(FourCc::NV12, 320, 240, 30),
        ],
    );
    let req = FrameRequirements::luma()
        .output_resolution(320, 180)
        .pyramid(1);
    let plan = plan_frames_with(&dev, &req, &registry()).unwrap();
    // 16:10 mode: 320 wide covers 180 high at 320x200, smaller than the 4:3 mode.
    assert_eq!(plan.mode.format.resolution.width.get(), 1280);
    assert_eq!(plan.output_resolution(), (320, 200));
    assert_eq!(plan.isp_output, Some((320, 200)));
    let scale = plan
        .steps
        .iter()
        .find(|s| s.kind == StepKind::Scale)
        .unwrap();
    assert_eq!(scale.execution, StepExecution::Hardware);
    // The ISP's pyramid level follows the scaled output.
    assert!(plan.to_string().contains("160x100"), "{plan}");

    let cpu_only = req.overrides(PlanOverrides {
        hardware: HardwarePolicy::Disabled,
        ..Default::default()
    });
    let plan = plan_frames_with(&dev, &cpu_only, &registry()).unwrap();
    assert_eq!(plan.isp_output, None);
}
