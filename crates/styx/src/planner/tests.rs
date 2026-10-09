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
    let plan = plan_frames_with(&dev, &Frames::gray().pyramid(2), &registry()).unwrap();
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
    let req = Frames::gray().size_at_least(1200, 700).fps_at_least(25);
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
                .any(|r| r.reason.contains("cannot run at 25 fps or faster"))
        );
    }
    #[cfg(not(feature = "codec-turbojpeg"))]
    assert!(plan.is_err());
}

#[test]
fn hardware_required_rejects_cpu_paths() {
    let req = Frames::gray().hardware(Hardware::Required);
    let err = plan_frames_with(&usb_camera(), &req, &registry()).unwrap_err();
    let PlanError::NoCandidates { rejected } = err else {
        panic!("expected NoCandidates, got {err:?}");
    };
    assert!(rejected.iter().any(|r| r.reason.contains("hardware")));
}

#[test]
fn forbidding_the_only_decoder_leaves_raw_paths() {
    let req = Frames::gray().forbid("turbojpeg-luma");
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
        Frames::gray().size(320, 180),
        Frames::gray().size_at_least(1280, 720),
    ];
    let plan = plan_many_with(&camera, &reqs, &registry()).unwrap();
    assert_eq!(plan.mode.format.resolution.width.get(), 1280);
    assert_eq!(plan.consumers.len(), 2);
    assert!(plan.consumers.iter().all(|c| c.mode.id == plan.mode.id));
    assert!(matches!(
        plan_many_with::<FrameRequest>(&camera, &[], &registry()),
        Err(PlanError::NoConsumers)
    ));
}

#[test]
fn delivery_sets_queue_depth_and_decode_threads() {
    let dev = device(
        BackendKind::Virtual,
        BackendHandle::Virtual,
        vec![mode(FourCc::NV12, 640, 480, 30)],
    );
    let latest = plan_frames_with(&dev, &Frames::gray(), &registry()).unwrap();
    assert_eq!(latest.queue_depth, 1);
    assert_eq!(latest.decode_threads, 0);
    let every = plan_frames_with(&dev, &Frames::gray().every_frame(8), &registry()).unwrap();
    assert_eq!(every.queue_depth, 8);
    assert_eq!(every.decode_threads, 1);
    let threads = Frames::gray().every_frame(8).decode_threads(0);
    let plan = plan_frames_with(&dev, &threads, &registry()).unwrap();
    assert_eq!((plan.queue_depth, plan.decode_threads), (8, 0));
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
    let plan = plan_frames_with(&dev, &Frames::gray().pyramid(2), &registry()).unwrap();
    assert_eq!(plan.isp_pyramid_level, Some(1));
    let pyramid: Vec<StepExecution> = plan
        .steps
        .iter()
        .filter(|s| matches!(s.kind, StepKind::Pyramid { .. }))
        .map(|s| s.execution)
        .collect();
    assert_eq!(pyramid, vec![StepExecution::Hardware, StepExecution::Cpu]);

    let hardware_only = Frames::gray()
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
    let req = Frames::gray().size(320, 180);
    let plan = plan_frames_with(&dev, &req, &registry());
    #[cfg(feature = "codec-turbojpeg")]
    {
        let plan = plan.unwrap();
        // 160x90 is too small; 1280x720 is the smallest mode covering 320x180, decoded at ¼.
        assert_eq!(plan.mode.format.resolution.width.get(), 1280);
        assert_eq!(plan.output_resolution(), (320, 180));
        let text = plan.to_string();
        assert!(text.contains("at 1/4 size (320x180)"), "{text}");

        let full = plan_frames_with(&dev, &Frames::gray(), &registry()).unwrap();
        assert!(plan.total.cpu_ms < full.total.cpu_ms * 0.5);

        // RGB through turbojpeg scales the same way.
        let rgb = Frames::formats([FourCc::RG24]).size(640, 360);
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
    let req = Frames::gray().size(4000, 3000);
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
    let req = Frames::gray().size(320, 180).pyramid(1);
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

    let cpu_only = req.hardware(Hardware::Off);
    let plan = plan_frames_with(&dev, &cpu_only, &registry()).unwrap();
    assert_eq!(plan.isp_output, None);
}

#[cfg(feature = "libcamera")]
#[test]
fn shared_consumers_of_two_sizes_use_both_isp_outputs() {
    let dev = device(
        BackendKind::Libcamera,
        BackendHandle::Libcamera {
            id: "/base/axi/pcie@1000120000/rp1/i2c@88000/ov9782@60".into(),
        },
        vec![mode(FourCc::NV12, 1280, 800, 30)],
    );
    let detector = Frames::gray().size(320, 180);
    let viewer = Frames::gray().size(640, 360);
    let plan = plan_many_with(&dev, &[detector.clone(), viewer], &registry()).unwrap();
    // The larger size on the main output, the smaller one on the second.
    let (small, large) = (&plan.consumers[0], &plan.consumers[1]);
    assert_eq!(small.output_resolution(), (320, 200));
    assert!(small.isp_second_output);
    assert!(small.to_string().contains("second output"), "{plan}");
    assert_eq!(large.output_resolution(), (640, 400));
    assert!(!large.isp_second_output);

    // A third size does not fit: those consumers get the mode's size.
    let third = Frames::gray().size(160, 90);
    let plan = plan_many_with(
        &dev,
        &[detector, Frames::gray().size(640, 360), third],
        &registry(),
    )
    .unwrap();
    assert!(plan.consumers.iter().all(|c| !c.isp_second_output));
    assert!(
        plan.consumers
            .iter()
            .all(|c| c.output_resolution() == (1280, 800))
    );
}

#[cfg(feature = "libcamera")]
#[test]
fn shared_plans_scale_with_the_isp_like_single_plans() {
    let ov9782 = |modes| {
        device(
            BackendKind::Libcamera,
            BackendHandle::Libcamera {
                id: "/base/axi/pcie@1000120000/rp1/i2c@88000/ov9782@60".into(),
            },
            modes,
        )
    };
    // Alone: the wide mode scaled by the ISP, not the smaller 4:3 mode.
    let dev = ov9782(vec![
        mode(FourCc::NV12, 1280, 800, 30),
        mode(FourCc::NV12, 320, 240, 30),
    ]);
    let detector = Frames::gray().size(320, 180);
    let plan = plan_many_with(&dev, std::slice::from_ref(&detector), &registry()).unwrap();
    assert_eq!(plan.consumers[0].output_resolution(), (320, 200), "{plan}");

    // Next to an RGB consumer on a YUYV capture, the luma decode gets the ISP's second output.
    let dev = ov9782(vec![mode(FourCc::YUYV, 1280, 800, 30)]);
    let plan = plan_many_with(
        &dev,
        &[detector, Frames::formats([FourCc::RG24])],
        &registry(),
    )
    .unwrap();
    assert_eq!(plan.consumers[0].output_resolution(), (320, 200), "{plan}");
    assert!(plan.consumers[0].isp_second_output, "{plan}");
    assert_eq!(plan.consumers[1].output_resolution(), (1280, 800));
}

#[cfg(feature = "native")]
#[test]
fn native_pisp_serves_two_sizes_and_formats_from_one_pass() {
    let mut dev = device(
        BackendKind::Native,
        BackendHandle::Native {
            key: "bridge:/dev/v4l-subdev2".into(),
        },
        vec![
            mode(FourCc::new(*b"pBAA"), 1280, 800, 30),
            mode(FourCc::NV12, 1280, 800, 30),
            mode(FourCc::RG24, 1280, 800, 30),
        ],
    );
    dev.backends[0].properties = vec![("isp".into(), "pisp".into())];
    let viewer = Frames::formats([FourCc::NV12]);
    let detector = Frames::formats([FourCc::RG24]).size(640, 400);
    let plan = plan_many_with(&dev, &[viewer.clone(), detector.clone()], &registry()).unwrap();
    assert_eq!(plan.mode.format.code, FourCc::NV12, "{plan}");
    let (main, second) = (&plan.consumers[0], &plan.consumers[1]);
    assert!(
        !main.isp_second_output && main.isp_format.is_none(),
        "{plan}"
    );
    assert_eq!(main.output_resolution(), (1280, 800));
    // RGB at 640x400 from the second output: no conversion, no CPU scaling.
    assert!(second.isp_second_output, "{plan}");
    assert_eq!(second.isp_format, Some(FourCc::RG24));
    assert_eq!(second.output_resolution(), (640, 400));
    assert!(second.total.cpu_ms < 2.0, "{plan}");
    let key = plan.setup_key();
    assert!(
        key.contains("second=Some((Some((640, 400)), Some("),
        "{key}"
    );

    // The same size in both formats: two outputs as well.
    let rgb = Frames::formats([FourCc::RG24]);
    let plan = plan_many_with(&dev, &[viewer.clone(), rgb.clone()], &registry()).unwrap();
    assert!(plan.consumers[1].isp_second_output, "{plan}");
    assert_eq!(plan.consumers[1].isp_format, Some(FourCc::RG24));

    // A third output does not fit: the sizes go first (all at the mode's size), the RGB
    // consumer keeps its format on the second output.
    let small_nv12 = Frames::formats([FourCc::NV12]).size(320, 200);
    let plan = plan_many_with(&dev, &[viewer, detector, small_nv12], &registry()).unwrap();
    assert!(
        plan.consumers
            .iter()
            .all(|c| c.output_resolution() == (1280, 800)),
        "{plan}"
    );
    assert!(plan.consumers[1].isp_second_output && plan.consumers[1].isp_format.is_some());
    assert!(!plan.consumers[2].isp_second_output, "{plan}");
    // Alone, an RGB consumer takes the RGB mode itself.
    let plan = plan_many_with(&dev, std::slice::from_ref(&rgb), &registry()).unwrap();
    assert_eq!(plan.mode.format.code, FourCc::RG24, "{plan}");
    assert!(plan.consumers[0].isp_format.is_none());

    // A sensor Styx drives runs at any rate in its range: exactly the rate asked.
    for m in &mut dev.backends[0].descriptor.modes {
        m.interval_stepwise = Some(IntervalStepwise {
            min: Interval::from_fps(120).unwrap(),
            max: Interval::from_fps(5).unwrap(),
            step: Interval::new(1000, 1).unwrap(),
        });
    }
    let slow = rgb.fps(30);
    let plan = plan_many_with(&dev, &[slow], &registry()).unwrap();
    assert_eq!(plan.interval, Interval::from_fps(30), "{plan}");
}

#[cfg(feature = "native")]
#[test]
fn native_pisp_supplies_the_first_pyramid_level() {
    let mut dev = device(
        BackendKind::Native,
        BackendHandle::Native {
            key: "bridge:/dev/v4l-subdev2".into(),
        },
        vec![
            mode(FourCc::new(*b"pBAA"), 1280, 800, 30),
            mode(FourCc::NV12, 1280, 800, 30),
            mode(FourCc::RG24, 1280, 800, 30),
        ],
    );
    dev.backends[0].properties = vec![("isp".into(), "pisp".into())];
    let plan = plan_frames_with(&dev, &Frames::gray().pyramid(2), &registry()).unwrap();
    assert_eq!(plan.mode.format.code, FourCc::NV12, "{plan}");
    assert_eq!(plan.isp_pyramid_level, Some(1), "{plan}");
    let pyramid: Vec<StepExecution> = plan
        .steps
        .iter()
        .filter(|s| matches!(s.kind, StepKind::Pyramid { .. }))
        .map(|s| s.execution)
        .collect();
    assert_eq!(pyramid, vec![StepExecution::Hardware, StepExecution::Cpu]);
    let hardware = Frames::formats([FourCc::NV12])
        .pyramid(1)
        .pyramid_source(PyramidSource::HardwareOnly);
    let plan = plan_frames_with(&dev, &hardware, &registry()).unwrap();
    assert_eq!(plan.isp_pyramid_level, Some(1), "{plan}");
    // Shared with a plain consumer: the capture is started with the pyramid level.
    let viewer = Frames::formats([FourCc::NV12]);
    let shared = plan_many_with(&dev, &[hardware.clone(), viewer], &registry()).unwrap();
    assert!(shared.setup_key().contains("pyramid=Some(1)"), "{shared}");
    // No PiSP (the software ISP): box filters only.
    dev.backends[0].properties = vec![("isp".into(), "software".into())];
    assert!(plan_frames_with(&dev, &hardware, &registry()).is_err());
}

#[cfg(feature = "native")]
#[test]
fn native_pisp_crops_the_region_and_makes_the_overview() {
    let mut dev = device(
        BackendKind::Native,
        BackendHandle::Native {
            key: "bridge:/dev/v4l-subdev2".into(),
        },
        vec![
            mode(FourCc::new(*b"pBAA"), 1280, 800, 30),
            mode(FourCc::NV12, 1280, 800, 30),
        ],
    );
    dev.backends[0].properties = vec![("isp".into(), "pisp".into())];
    let region = FrameRect::new(100, 50, 320, 200);
    let tracker = Frames::gray().roi(region).overview(320, 200).pyramid(1);
    let plan = plan_frames_with(&dev, &tracker, &registry()).unwrap();
    let d = plan.delivered();
    assert_eq!(d.roi, Some(RoiCrop::Isp), "{plan}");
    assert_eq!((d.overview, d.hardware_overview), (Some((320, 200)), true));
    assert!(d.unmet.is_empty(), "{:?}", d.unmet);
    // The second output makes the overview: the pyramid (of the crop) is box-filtered.
    assert_eq!(plan.isp_pyramid_level, None, "{plan}");
    let config = plan.region_config(Default::default());
    assert_eq!(config.backends.native.crop, Some(region));
    assert_eq!(config.backends.native.overview, Some((320, 200)));
    // Any format: the ISP crops NV12 too.
    let nv12 = plan_frames_with(&dev, &Frames::nv12().roi(region), &registry()).unwrap();
    assert_eq!(nv12.delivered().roi, Some(RoiCrop::Isp), "{nv12}");
    assert_eq!(nv12.delivered().overview, None);
    // Alone on a shared capture (a camera service's one client): still the ISP's, and the
    // capture is set up for it.
    let alone = plan_many_with(&dev, std::slice::from_ref(&tracker), &registry()).unwrap();
    assert_eq!(alone.consumers[0].delivered().roi, Some(RoiCrop::Isp));
    assert!(alone.setup_key().contains("region=[(["), "{alone}");
    // Shared with a viewer of the whole frame: views of it, the overview from the ISP.
    let shared = plan_many_with(&dev, &[tracker.clone(), Frames::nv12()], &registry()).unwrap();
    let d = shared.consumers[0].delivered();
    assert_eq!(d.roi, Some(RoiCrop::View), "{shared}");
    assert_eq!((d.overview, d.hardware_overview), (Some((320, 200)), true));
    assert!(d.unmet.is_empty(), "{:?}", d.unmet);
    // A hardware-only pyramid of the region: from an extra pass of it.
    let pyramid = Frames::gray()
        .roi(region)
        .pyramid(1)
        .pyramid_source(PyramidSource::HardwareOnly);
    let plan = plan_frames_with(&dev, &pyramid, &registry()).unwrap();
    assert_eq!(plan.delivered().roi, Some(RoiCrop::Isp), "{plan}");
    assert_eq!(plan.isp_pyramid_level, Some(1));
}

#[cfg(feature = "native")]
#[test]
fn native_software_isp_processes_only_the_region_and_bins_the_overview() {
    let mut dev = device(
        BackendKind::Native,
        BackendHandle::Native {
            key: "bridge:/dev/v4l-subdev2".into(),
        },
        vec![
            mode(FourCc::new(*b"pBAA"), 1280, 800, 30),
            mode(FourCc::NV12, 1280, 800, 30),
            mode(FourCc::NV12, 640, 400, 30),
        ],
    );
    dev.backends[0].properties = vec![("isp".into(), "software".into())];
    let whole = plan_frames_with(&dev, &Frames::nv12(), &registry()).unwrap();
    // With `gpu-isp` on a host with a Vulkan GPU the GPU ISP runs the path: no regions.
    if whole.to_string().contains("GPU ISP") {
        return;
    }
    let region = FrameRect::new(100, 50, 320, 200);
    let tracker = Frames::nv12().roi(region).overview(320, 200);
    let plan = plan_frames_with(&dev, &tracker, &registry()).unwrap();
    let d = plan.delivered();
    assert_eq!(d.roi, Some(RoiCrop::Isp), "{plan}");
    assert_eq!((d.overview, d.hardware_overview), (Some((320, 200)), true));
    assert!(d.unmet.is_empty(), "{:?}", d.unmet);
    assert!(plan.to_string().contains("binned 1/4"), "{plan}");
    // Priced for the region and the overview: a fraction of the whole frame (CM5: 1.3
    // against 3.2 ms).
    assert!(
        plan.total.cpu_ms < 0.6 * whole.total.cpu_ms,
        "{plan} vs {whole}"
    );
    let config = plan.region_config(Default::default());
    assert_eq!(config.backends.native.crop, Some(region));
    assert_eq!(config.backends.native.overview, Some((320, 200)));
    // Luma too (a view of the region's Y plane); without an overview, the statistics alone.
    let gray = plan_frames_with(&dev, &Frames::gray().roi(region), &registry()).unwrap();
    assert_eq!(gray.delivered().roi, Some(RoiCrop::Isp), "{gray}");
    assert!(gray.total.cpu_ms < plan.total.cpu_ms, "{gray} vs {plan}");
    // Shared with another consumer: the whole frame, priced as such; the overview box-filtered
    // from it on the CPU.
    let shared = plan_many_with(&dev, &[tracker.clone(), Frames::nv12()], &registry()).unwrap();
    let d = shared.consumers[0].delivered();
    assert_eq!((d.roi, d.overview), (None, Some((320, 200))), "{shared}");
    assert!(!d.hardware_overview);
    assert!(
        shared.consumers[0].total.cpu_ms >= whole.total.cpu_ms,
        "{shared}"
    );
    // A binned mode (frames of half size) is not cropped: unmet.
    let small = Frames::nv12().size(640, 400).roi(region);
    let plan = plan_frames_with(&dev, &small, &registry()).unwrap();
    assert_eq!(plan.mode.format.resolution.width.get(), 640, "{plan}");
    assert_eq!(plan.delivered().roi, None, "{plan}");
    assert_eq!(plan.delivered().unmet, vec![Unmet::Roi]);
    assert!(plan_frames_with(&dev, &small.strict(), &registry()).is_err());
}

#[cfg(feature = "libcamera")]
#[test]
fn raspberry_pi_isp_crops_the_region_through_libcamera() {
    let mut dev = device(
        BackendKind::Libcamera,
        BackendHandle::Libcamera {
            id: "/base/axi/pcie@1000120000/rp1/i2c@88000/ov9782@60".into(),
        },
        vec![mode(FourCc::NV12, 1280, 800, 30)],
    );
    let region = FrameRect::new(101, 50, 319, 200);
    let tracker = Frames::gray().roi(region).overview(320, 200);
    // Without the per-output crop control: views of the frame.
    let plan = plan_frames_with(&dev, &tracker, &registry()).unwrap();
    assert_eq!(plan.delivered().roi, Some(RoiCrop::View), "{plan}");
    let rect = ControlValue::Rects(Vec::new());
    dev.backends[0].descriptor.controls.push(ControlMeta {
        id: ControlId(20003),
        name: "ScalerCrops".into(),
        kind: ControlKind::Rectangle,
        access: Access::ReadWrite,
        min: rect.clone(),
        max: rect.clone(),
        default: rect,
        step: None,
        menu: None,
        metadata: Default::default(),
    });
    let plan = plan_frames_with(&dev, &tracker, &registry()).unwrap();
    let d = plan.delivered();
    assert_eq!(d.roi, Some(RoiCrop::Isp), "{plan}");
    assert_eq!((d.overview, d.hardware_overview), (Some((320, 200)), true));
    assert!(plan.to_string().contains("ScalerCrops"), "{plan}");
    // The main output at the region's size (even), the second the overview.
    let lc = plan.region_config(Default::default()).backends.libcamera;
    let even = FrameRect::new(100, 50, 320, 200);
    assert_eq!((lc.crop, lc.output_size), (Some(even), Some((320, 200))));
    assert_eq!(
        (lc.second_output_size, lc.overview),
        (Some((320, 200)), true)
    );
    // An overview alone sizes nothing: the uncropped frame.
    let plan = plan_frames_with(&dev, &Frames::gray().overview(320, 200), &registry()).unwrap();
    assert!(!plan.delivered().hardware_overview, "{plan}");
}

/// The OV9782 on the PiSP as libcamera 0.7 offers it: the camera helper aliases it to the mono
/// OV9281, so the raw role offers R8 (the Bayer mosaic) next to the ISP's NV12.
#[cfg(feature = "libcamera")]
fn pisp_ov9782(modes: Vec<Mode>) -> ProbedDevice {
    device(
        BackendKind::Libcamera,
        BackendHandle::Libcamera {
            id: "/base/axi/pcie@1000120000/rp1/i2c@88000/ov9782@60".into(),
        },
        modes,
    )
}

#[cfg(feature = "libcamera")]
#[test]
fn grey_from_an_isp_camera_is_the_processed_y_plane_not_the_raw_stream() {
    let dev = pisp_ov9782(vec![
        mode(FourCc::R8, 1280, 800, 30),
        mode(FourCc::NV12, 1280, 800, 30),
    ]);
    let plan = plan_frames_with(&dev, &Frames::gray().every_frame(8), &registry()).unwrap();
    assert_eq!(plan.mode.format.code, FourCc::NV12, "{plan}");
    assert!(matches!(plan.route, Route::LumaView));
    assert!(!plan.raw_sensor_stream());
    assert_eq!(plan.delivered().format, FourCc::GREY);
    assert!(
        plan.rejected
            .iter()
            .any(|r| r.candidate.contains("R8") && r.reason.contains("raw sensor stream")),
        "{plan}"
    );

    // The raw stream when asked for by format, and the plan says what it is.
    let raw = plan_frames_with(&dev, &Frames::formats([FourCc::R8]), &registry()).unwrap();
    assert_eq!(raw.mode.format.code, FourCc::R8);
    assert!(matches!(raw.route, Route::Direct));
    assert!(raw.raw_sensor_stream());
    assert!(raw.to_string().contains("raw sensor stream"), "{raw}");

    // Without a processed mode, the raw stream is the only grey there is.
    let only_raw = pisp_ov9782(vec![mode(FourCc::R8, 1280, 800, 30)]);
    let plan = plan_frames_with(&only_raw, &Frames::gray(), &registry()).unwrap();
    assert_eq!(plan.mode.format.code, FourCc::R8);
}

#[test]
fn grey_cameras_without_an_isp_deliver_their_grey_frames() {
    // A mono USB camera (or any camera without an ISP): GREY is its picture, taken as it is.
    for code in [FourCc::GREY, FourCc::R8] {
        let dev = device(
            BackendKind::Virtual,
            BackendHandle::Virtual,
            vec![mode(code, 1280, 800, 30), mode(FourCc::NV12, 1280, 800, 30)],
        );
        let plan = plan_frames_with(&dev, &Frames::gray(), &registry()).unwrap();
        assert!(
            !plan
                .rejected
                .iter()
                .any(|r| r.reason.contains("raw sensor stream")),
            "{plan}"
        );
        assert!(!plan.raw_sensor_stream());
        let grey = device(
            BackendKind::Virtual,
            BackendHandle::Virtual,
            vec![mode(code, 1280, 800, 30)],
        );
        let plan = plan_frames_with(&grey, &Frames::gray(), &registry()).unwrap();
        assert_eq!(plan.mode.format.code, code);
        assert!(matches!(plan.route, Route::Direct));
    }
}
