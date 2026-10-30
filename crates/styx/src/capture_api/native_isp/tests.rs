//! Tests of the processed native capture's modes and set-up.

use super::*;

#[test]
fn processed_modes_follow_the_raw_sizes() {
    let res = Resolution::new(1280, 800).unwrap();
    let raw = |code: &[u8; 4]| {
        let format = MediaFormat::new(FourCc::new(*code), res, ColorSpace::Unknown);
        Mode {
            id: ModeId {
                format,
                interval: None,
            },
            format,
            intervals: smallvec![Interval::from_fps(30).unwrap()],
            interval_stepwise: None,
        }
    };
    let modes = processed_modes(&[raw(b"pBAA"), raw(b"BA81")], false);
    let codes: Vec<String> = modes.iter().map(|m| m.format.code.to_string()).collect();
    assert_eq!(codes, ["NV12", "RG24"]);
    let binned = processed_modes(&[raw(b"pBAA")], true);
    let sizes: Vec<(u32, u32)> = binned
        .iter()
        .map(|m| {
            (
                m.format.resolution.width.get(),
                m.format.resolution.height.get(),
            )
        })
        .collect();
    assert_eq!(sizes, [(1280, 800), (1280, 800), (640, 400), (640, 400)]);
    assert!(modes.iter().all(|m| m.intervals.len() == 1));
    assert!(is_processed(FourCc::NV12) && !is_processed(FourCc::new(*b"pBAA")));
    let l = layouts(FourCc::NV12, 800, 1280);
    assert_eq!((l[1].offset, l[1].len), (1280 * 800, 1280 * 400));
}

fn device(isp: &str) -> crate::ProbedDevice {
    use crate::{BackendHandle, DeviceIdentity, ProbedBackend};
    let res = Resolution::new(1280, 800).unwrap();
    let format = MediaFormat::new(FourCc::new(*b"pBAA"), res, ColorSpace::Unknown);
    let raw = Mode {
        id: ModeId {
            format,
            interval: None,
        },
        format,
        intervals: smallvec![Interval::from_fps(30).unwrap()],
        interval_stepwise: None,
    };
    let mut modes = vec![raw.clone()];
    modes.extend(processed_modes(&[raw], isp == "software"));
    crate::ProbedDevice {
        identity: DeviceIdentity {
            display: "ov9782".into(),
            keys: vec!["native:ov9782".into()],
        },
        backends: vec![ProbedBackend {
            kind: BackendKind::Native,
            handle: BackendHandle::Native {
                key: "bridge:/dev/v4l-subdev2".into(),
            },
            descriptor: CaptureDescriptor::new(modes),
            properties: vec![("isp".into(), isp.into())],
        }],
    }
}

#[test]
fn the_planner_picks_the_isp_for_processed_frames() {
    let pisp = device("pisp");
    for (req, code, luma) in [
        (
            crate::planner::Frames::formats([FourCc::NV12]),
            FourCc::NV12,
            false,
        ),
        (
            crate::planner::Frames::formats([FourCc::RG24]),
            FourCc::RG24,
            false,
        ),
        (crate::planner::Frames::gray(), FourCc::NV12, true),
    ] {
        let plan = crate::planner::plan_frames(&pisp, &req).unwrap();
        assert_eq!(plan.backend, BackendKind::Native);
        assert_eq!(plan.mode.format.code, code, "{plan}");
        assert!(plan.to_string().contains("PiSP"), "{plan}");
        assert!(plan.total.cpu_ms < 2.0, "{plan}");
        assert_eq!(
            plan.steps
                .iter()
                .any(|s| s.kind == crate::planner::StepKind::LumaView),
            luma,
            "{plan}"
        );
        // The raw mode through a decoder is not offered (no 3A on that route).
        assert!(
            plan.rejected.iter().any(|r| r.reason.contains("3A")),
            "{plan}"
        );
    }
    // Raw stays raw.
    let raw = crate::planner::Frames::formats([FourCc::new(*b"pBAA")]);
    let plan = crate::planner::plan_frames(&pisp, &raw).unwrap();
    assert_eq!(plan.mode.format.code, FourCc::new(*b"pBAA"));
    // Without a PiSP the software ISP runs the loop, priced by the megapixel.
    let soft = device("software");
    let plan = crate::planner::plan_frames(&soft, &crate::planner::Frames::formats([FourCc::NV12]))
        .unwrap();
    // With `gpu-isp` on a host with a Vulkan GPU the GPU ISP takes that route instead.
    if plan.to_string().contains("GPU ISP") {
        return;
    }
    assert!(plan.to_string().contains("software ISP"), "{plan}");
    assert!(plan.total.cpu_ms > 2.5, "{plan}");
    assert_eq!(plan.mode.format.resolution.width.get(), 1280, "{plan}");
    // A consumer of small frames gets the binned mode: no demosaic, less than half the CPU.
    let small = crate::planner::Frames::formats([FourCc::NV12]).size(640, 400);
    let half = crate::planner::plan_frames(&soft, &small).unwrap();
    assert_eq!(half.mode.format.resolution.width.get(), 640, "{half}");
    assert!(
        half.total.cpu_ms < plan.total.cpu_ms * 0.6,
        "{half} vs {plan}"
    );
}
