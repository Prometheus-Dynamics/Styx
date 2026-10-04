//! Frame rates, delivery, shared captures with mixed requests, and the deprecated request.

use smallvec::smallvec;
use styx_capture::{CaptureDescriptor, ModeId};

use super::*;
use crate::DeviceIdentity;
use crate::prelude::{BackendHandle, ProbedBackend};

fn listed(code: FourCc, width: u32, height: u32, rates: &[u32]) -> Mode {
    let format = MediaFormat::with_default_color(code, Resolution::new(width, height).unwrap());
    Mode {
        id: ModeId {
            format,
            interval: None,
        },
        format,
        intervals: rates
            .iter()
            .map(|&fps| Interval::from_fps(fps).unwrap())
            .collect(),
        interval_stepwise: None,
    }
}

/// A mode that runs at any rate from `slowest` to `fastest` (a sensor Styx drives).
fn ranged(code: FourCc, width: u32, height: u32, slowest: u32, fastest: u32) -> Mode {
    let mut mode = listed(code, width, height, &[]);
    mode.intervals = smallvec![Interval::from_fps(fastest).unwrap()];
    mode.interval_stepwise = Some(IntervalStepwise {
        min: Interval::from_fps(fastest).unwrap(),
        max: Interval::from_fps(slowest).unwrap(),
        step: Interval::new(1000, 1).unwrap(),
    });
    mode
}

fn camera(modes: Vec<Mode>) -> ProbedDevice {
    ProbedDevice {
        identity: DeviceIdentity {
            display: "test-camera".into(),
            keys: Vec::new(),
        },
        backends: vec![ProbedBackend {
            kind: BackendKind::Virtual,
            handle: BackendHandle::Virtual,
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

fn usb() -> ProbedDevice {
    camera(vec![
        listed(FourCc::NV12, 1280, 720, &[30, 25, 15, 10, 5]),
        listed(FourCc::NV12, 640, 480, &[60, 30, 15]),
    ])
}

fn sensor() -> ProbedDevice {
    camera(vec![
        ranged(FourCc::NV12, 1280, 800, 2, 120),
        ranged(FourCc::NV12, 640, 400, 2, 260),
    ])
}

fn fps(plan: &FramePlan) -> f32 {
    plan.interval
        .map_or(0.0, |i| (i.fps() * 100.0).round() / 100.0)
}

fn plan(device: &ProbedDevice, request: FrameRequest) -> Result<FramePlan, PlanError> {
    plan_frames_with(device, &request, &registry())
}

#[test]
fn exact_rate_on_a_range_camera_is_that_rate() {
    for rate in [30, 45, 90] {
        let p = plan(&sensor(), Frames::nv12().fps(rate)).unwrap();
        assert_eq!(fps(&p), rate as f32, "{p}");
        assert_eq!(p.mode.format.resolution.width.get(), 1280);
    }
    // 200 fps only at 640x400.
    let p = plan(&sensor(), Frames::nv12().fps(200)).unwrap();
    assert_eq!(
        (fps(&p), p.mode.format.resolution.width.get()),
        (200.0, 640)
    );
}

#[test]
fn exact_rate_on_a_list_camera_is_a_listed_rate() {
    let p = plan(&usb(), Frames::nv12().fps(15)).unwrap();
    assert_eq!(fps(&p), 15.0);
    assert_eq!(p.mode.format.resolution.width.get(), 1280);
    // Only the 640x480 mode lists 60 fps.
    let p = plan(&usb(), Frames::nv12().fps(60)).unwrap();
    assert_eq!((fps(&p), p.mode.format.resolution.width.get()), (60.0, 640));
}

#[test]
fn exact_rate_no_mode_has_names_the_rates_there_are() {
    let err = plan(&usb(), Frames::nv12().fps(50)).unwrap_err();
    let PlanError::FrameRate(message) = &err else {
        panic!("expected a frame rate error, got {err:?}");
    };
    assert!(message.contains("exactly 50 fps"), "{message}");
    assert!(message.contains("30, 25, 15, 10, 5 fps"), "{message}");
    assert!(message.contains("60, 30, 15 fps"), "{message}");
    let err = plan(&sensor(), Frames::nv12().fps(300)).unwrap_err();
    assert!(err.to_string().contains("any rate 2.0..260.0 fps"), "{err}");
    // A mode that could not deliver the frames anyway is not blamed on its rate.
    let err = plan(&usb(), Frames::nv12().size_at_least(4000, 3000).fps(50)).unwrap_err();
    assert!(matches!(err, PlanError::NoCandidates { .. }), "{err}");
}

#[test]
fn at_least_takes_the_fastest_rate() {
    let p = plan(&usb(), Frames::nv12().fps_at_least(20)).unwrap();
    assert_eq!(fps(&p), 30.0);
    let p = plan(&usb(), Frames::nv12().fps_at_least(40)).unwrap();
    assert_eq!((fps(&p), p.mode.format.resolution.width.get()), (60.0, 640));
    let p = plan(&sensor(), Frames::nv12().fps_at_least(30)).unwrap();
    assert_eq!(fps(&p), 120.0);
    assert!(matches!(
        plan(&usb(), Frames::nv12().fps_at_least(90)),
        Err(PlanError::FrameRate(_))
    ));
}

#[test]
fn between_takes_the_fastest_rate_within() {
    let p = plan(&usb(), Frames::nv12().fps_between(12, 28)).unwrap();
    assert_eq!(fps(&p), 25.0);
    let p = plan(&sensor(), Frames::nv12().fps_between(15, 60)).unwrap();
    assert_eq!(fps(&p), 60.0);
    // Reversed bounds are put in order.
    let p = plan(&sensor(), Frames::nv12().fps_between(60, 15)).unwrap();
    assert_eq!(fps(&p), 60.0);
    assert!(plan(&usb(), Frames::nv12().fps_between(16, 24)).is_err());
}

#[test]
fn no_rate_is_the_cameras_default() {
    // 30 within a range, not the range's fastest.
    let p = plan(&sensor(), Frames::nv12()).unwrap();
    assert_eq!(fps(&p), DEFAULT_FPS as f32);
    // The listed rate closest to 30.
    let p = plan(&usb(), Frames::nv12().size(640, 480)).unwrap();
    assert_eq!(fps(&p), 30.0);
    let slow = camera(vec![listed(FourCc::NV12, 640, 480, &[60, 15])]);
    assert_eq!(fps(&plan(&slow, Frames::nv12()).unwrap()), 15.0);
    let fast = camera(vec![ranged(FourCc::NV12, 640, 480, 50, 100)]);
    assert_eq!(fps(&plan(&fast, Frames::nv12()).unwrap()), 50.0);
}

#[test]
fn the_plan_says_how_it_ranks() {
    let text = plan(&sensor(), Frames::nv12()).unwrap().to_string();
    assert!(text.contains("= CPU + latency x 0.5"), "{text}");
}

#[test]
fn shared_capture_serves_mixed_requests() {
    let requests = [
        Frames::gray().size(320, 200).fps(30).every_frame(4),
        Frames::nv12().fps_at_least(15),
        Frames::gray().roi(FrameRect::new(0, 0, 64, 64)),
    ];
    let shared = plan_many_with(&sensor(), &requests, &registry()).unwrap();
    assert_eq!(shared.interval, Interval::from_fps(30), "{shared}");
    let depths: Vec<usize> = shared.consumers.iter().map(|c| c.queue_depth).collect();
    assert_eq!(depths, vec![4, 1, 1]);
    assert!(
        shared
            .consumers
            .iter()
            .all(|c| c.interval == shared.interval)
    );
    // Without an exact rate the fastest every consumer accepts.
    let shared = plan_many_with(
        &sensor(),
        &[
            Frames::nv12().fps_at_least(15),
            Frames::gray().fps_between(10, 50),
        ],
        &registry(),
    )
    .unwrap();
    assert_eq!(shared.interval, Interval::from_fps(50), "{shared}");
    // No rate at all: the camera's default.
    let shared = plan_many_with(&usb(), &[Frames::nv12(), Frames::gray()], &registry()).unwrap();
    assert_eq!(shared.interval.map(|i| i.fps().round()), Some(30.0));
    // Two different exact rates cannot share a capture.
    let err = plan_many_with(
        &sensor(),
        &[Frames::nv12().fps(30), Frames::gray().fps(60)],
        &registry(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("exactly 30 and 60 fps"), "{err}");
    // A rate one mode has, another not: the mode that has it.
    let shared = plan_many_with(
        &usb(),
        &[Frames::nv12().fps(60), Frames::gray()],
        &registry(),
    )
    .unwrap();
    assert_eq!(shared.mode.format.resolution.width.get(), 640, "{shared}");
}

#[test]
fn shared_consumers_differing_in_rate_or_roi_prepare_once() {
    let requests = [
        Frames::gray().fps_at_least(15),
        Frames::gray().roi(FrameRect::new(0, 0, 64, 64)),
        Frames::gray().every_frame(4),
    ];
    let shared = plan_many_with(&sensor(), &requests, &registry()).unwrap();
    assert!(
        shared.consumers[0]
            .notes
            .iter()
            .any(|n| n.contains("consumers 0, 1")),
        "{shared}"
    );
    assert!(
        !shared.consumers[2]
            .notes
            .iter()
            .any(|n| n.contains("share")),
        "{shared}"
    );
}

#[test]
fn camera_first_requests_are_the_same() {
    let usb = usb();
    let request = usb.frames().nv12().size(640, 480).fps(30).every_frame(2);
    assert_eq!(
        request.request(),
        &Frames::nv12().size(640, 480).fps(30).every_frame(2)
    );
    let plan = request.plan().unwrap();
    assert_eq!(plan.mode.format.resolution.width.get(), 640);
    assert_eq!(usb.frames().request(), &Frames::any());
}

#[allow(deprecated)]
mod deprecated {
    use super::*;

    #[test]
    fn requirements_convert_to_the_request_they_meant() {
        let old = FrameRequirements::formats([FourCc::NV12])
            .output_resolution(1280, 800)
            .min_fps(30)
            .priority(Priority::Power);
        assert_eq!(
            FrameRequest::from(&old),
            Frames::nv12().size(1280, 800).fps(30).every_frame(3)
        );
        let old = FrameRequirements::luma().min_fps(25).stride_alignment(64);
        assert_eq!(
            FrameRequest::from(&old),
            Frames::gray().fps_at_least(25).row_alignment(64)
        );
        let old = FrameRequirements::luma()
            .min_resolution(640, 480)
            .max_resolution(1920, 1080)
            .priority(Priority::Throughput);
        assert_eq!(
            FrameRequest::from(&old),
            Frames::gray()
                .size_at_least(640, 480)
                .size_at_most(1920, 1080)
                .every_frame(4)
        );
        // Overrides: a deeper queue keeps the latency priority's automatic threads.
        let old = FrameRequirements::luma().overrides(PlanOverrides {
            backend: Some("v4l2".into()),
            decoder: Some("turbojpeg-luma".into()),
            forbid: vec!["ffmpeg".into()],
            hardware: HardwarePolicy::Disabled,
            decode_threads: None,
            queue_depth: Some(6),
        });
        assert_eq!(
            FrameRequest::from(&old),
            Frames::gray()
                .backend(BackendKind::V4l2)
                .decoder("turbojpeg-luma")
                .forbid("ffmpeg")
                .hardware(Hardware::Off)
                .every_frame(6)
                .decode_threads(0)
        );
    }

    #[test]
    fn requirements_plan_as_before() {
        let cases = [
            // Power with a rate: exactly that rate on a range camera.
            (
                FrameRequirements::formats([FourCc::NV12])
                    .min_fps(45)
                    .priority(Priority::Power),
                45.0,
                3,
            ),
            // Latency with a rate: the fastest.
            (
                FrameRequirements::formats([FourCc::NV12]).min_fps(45),
                120.0,
                1,
            ),
            // No rate: 30 on a range camera.
            (
                FrameRequirements::luma().priority(Priority::Throughput),
                30.0,
                4,
            ),
        ];
        for (old, rate, depth) in cases {
            let p = plan_frames_with(&sensor(), &old, &registry()).unwrap();
            assert_eq!((fps(&p), p.queue_depth), (rate, depth), "{p}");
            let new = plan_frames_with(&sensor(), &FrameRequest::from(&old), &registry()).unwrap();
            assert_eq!(
                (
                    new.mode.id.clone(),
                    new.interval,
                    new.queue_depth,
                    new.decode_threads
                ),
                (
                    p.mode.id.clone(),
                    p.interval,
                    p.queue_depth,
                    p.decode_threads
                )
            );
        }
        // Several at once, as before.
        let shared = plan_many_with(
            &sensor(),
            &[
                FrameRequirements::luma()
                    .min_fps(30)
                    .priority(Priority::Power),
                FrameRequirements::formats([FourCc::NV12]),
            ],
            &registry(),
        )
        .unwrap();
        assert_eq!(shared.interval, Interval::from_fps(30));
    }
}

#[test]
fn plans_say_what_they_deliver_and_strict_requests_refuse_less() {
    let cam = camera(vec![
        listed(FourCc::NV12, 640, 360, &[30]),
        listed(FourCc::NV12, 1280, 720, &[30]),
    ]);
    // Nothing scales NV12 here: the smallest mode arrives, and the plan says so.
    let plan = plan_frames_with(&cam, &Frames::nv12().size(160, 90), &registry()).unwrap();
    let delivered = plan.delivered();
    assert_eq!(delivered.format, FourCc::NV12);
    assert_eq!(delivered.size, (640, 360));
    assert_eq!(delivered.fps, Some(30.0));
    assert_eq!(
        delivered.unmet,
        [Unmet::Size {
            wanted: (160, 90),
            delivered: (640, 360),
        }]
    );
    // A size the camera has is met.
    let plan = plan_frames_with(&cam, &Frames::nv12().size(640, 360), &registry()).unwrap();
    assert!(plan.delivered().unmet.is_empty());

    // Strict: no degraded plan, and the reasons name what is missing.
    let strict = Frames::nv12().size(160, 90).strict();
    match plan_frames_with(&cam, &strict, &registry()) {
        Err(PlanError::NoCandidates { rejected }) => assert!(
            rejected
                .iter()
                .all(|r| r.reason.contains("strict: frames are")),
            "{rejected:?}"
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }
    // Shared: a strict consumer refuses, a lenient one is told.
    assert!(plan_many_with(&cam, &[Frames::nv12(), strict.clone()], &registry()).is_err());
    let shared = plan_many_with(
        &cam,
        &[Frames::nv12(), Frames::nv12().size(160, 90)],
        &registry(),
    )
    .unwrap();
    assert_eq!(shared.consumers[1].unmet.len(), 1);
}
