//! Regions of interest across consumers: where each is made (main output, second output, extra
//! passes, views), the capture set-up, frames per consumer, and stale frames.

use smallvec::smallvec;
use styx_capture::{CaptureDescriptor, ModeId};

use super::region::IspPlace;
use super::*;
use crate::DeviceIdentity;
use crate::prelude::{BackendHandle, ProbedBackend};

fn pisp(isp: &str) -> ProbedDevice {
    let mode = |code: FourCc, w, h| {
        let format = MediaFormat::with_default_color(code, Resolution::new(w, h).unwrap());
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
    ProbedDevice {
        identity: DeviceIdentity {
            display: "ov9782".into(),
            keys: Vec::new(),
        },
        backends: vec![ProbedBackend {
            kind: BackendKind::Native,
            handle: BackendHandle::Native {
                key: "bridge:/dev/v4l-subdev2".into(),
            },
            descriptor: CaptureDescriptor {
                modes: vec![
                    mode(FourCc::new(*b"pBAA"), 1280, 800),
                    mode(FourCc::NV12, 1280, 800),
                ],
                controls: Vec::new(),
            },
            properties: vec![("isp".into(), isp.into())],
        }],
    }
}

fn registry() -> CodecRegistryHandle {
    CodecRegistry::with_enabled_codecs().unwrap().handle()
}

fn rect(x: u32) -> FrameRect {
    FrameRect::new(x, 64, 128, 128)
}

use RoiCrop::{Isp, IspPass, View};

/// One consumer: its first region is the main output's crop, the others extra passes; a
/// hardware-only pyramid comes from a pass of the region.
#[test]
fn one_consumer_crops_its_first_region_in_the_main_output_and_passes_the_others() {
    let dev = pisp("pisp");
    let req = Frames::nv12()
        .regions([rect(0), rect(200), rect(400)])
        .skip_stale_regions();
    let plan = plan_frames_with(&dev, &req, &registry()).unwrap();
    let d = plan.delivered();
    assert_eq!(d.roi, Some(Isp), "{plan}");
    assert_eq!(d.regions, vec![Some(Isp), Some(IspPass), Some(IspPass)]);
    assert!(d.unmet.is_empty(), "{:?}", d.unmet);
    let native = plan.region_config(Default::default()).backends.native;
    assert_eq!(native.crop, Some(rect(0)));
    assert_eq!(native.regions[0].unwrap().rect, Some(rect(200)));
    assert_eq!(native.regions[1].unwrap().rect, Some(rect(400)));
    assert_eq!(native.pass_regions().collect::<Vec<_>>(), [0, 1]);
    assert!(
        plan.to_string()
            .contains("2 more regions cropped by extra ISP passes"),
        "{plan}"
    );
    // A hardware-only pyramid of the region: one level, from an extra pass.
    let pyramid = Frames::gray()
        .roi(rect(0))
        .pyramid(1)
        .pyramid_source(PyramidSource::HardwareOnly);
    let plan = plan_frames_with(&dev, &pyramid, &registry()).unwrap();
    assert_eq!(plan.isp_pyramid_level, Some(1));
    assert_eq!(plan.delivered().roi, Some(Isp));
    let config = plan.region_config(StyxConfig::default().native_pyramid_level(1));
    assert!(config.backends.native.pyramid_pass());
    assert!(
        plan.to_string().contains("extra ISP pass of the region"),
        "{plan}"
    );
    // Without a PiSP, luma regions are views; others unmet.
    let soft = pisp("software");
    let views = plan_frames_with(
        &soft,
        &Frames::gray().regions([rect(0), rect(200)]),
        &registry(),
    )
    .unwrap();
    assert_eq!(views.delivered().regions, vec![Some(View), Some(View)]);
    let nv12 = plan_frames_with(&soft, &req, &registry()).unwrap();
    assert_eq!(nv12.delivered().unmet, vec![Unmet::Roi]);
}

/// Two trackers, nobody needs the whole frame: the first's region is the main output's crop,
/// the second's an extra pass; neither restarts the capture when its region moves.
#[test]
fn two_trackers_share_one_pass_and_an_extra_one() {
    let dev = pisp("pisp");
    let a = Frames::gray().roi(rect(0)).overview(320, 200);
    let b = Frames::gray().roi(rect(600)).overview(640, 400);
    let shared = plan_many_with(&dev, &[a.clone(), b.clone()], &registry()).unwrap();
    let (da, db) = (
        shared.consumers[0].delivered(),
        shared.consumers[1].delivered(),
    );
    assert_eq!(da.regions, vec![Some(Isp)], "{shared}");
    assert_eq!(db.regions, vec![Some(IspPass)], "{shared}");
    // One overview for both, the larger.
    assert_eq!(
        (da.overview, da.hardware_overview),
        (Some((640, 400)), true)
    );
    assert_eq!(db.overview, Some((640, 400)));
    assert_eq!(
        shared.consumers[1].region.places,
        vec![Some(IspPlace::Slot(0))]
    );
    let key = shared.setup_key();
    let moved = plan_many_with(&dev, &[a.roi(rect(300)), b.roi(rect(900))], &registry()).unwrap();
    assert_eq!(moved.setup_key(), key, "regions move without a restart");
}

/// A viewer takes the whole frame: a luma tracker's regions are views of it (free), an NV12
/// tracker's the second output's crop (free: its pass covers the frame) and extra passes.
#[test]
fn with_a_viewer_regions_are_views_the_second_output_and_passes() {
    let dev = pisp("pisp");
    let viewer = Frames::nv12();
    let nv12 = Frames::nv12().regions([rect(0), rect(300)]);
    let gray = Frames::gray().regions([rect(600), rect(900)]);
    let shared = plan_many_with(&dev, &[viewer, nv12, gray], &registry()).unwrap();
    let d: Vec<_> = shared.consumers.iter().map(|p| p.delivered()).collect();
    assert_eq!(d[0].regions, Vec::<Option<RoiCrop>>::new());
    assert_eq!(d[1].regions, vec![Some(Isp), Some(IspPass)], "{shared}");
    assert_eq!(d[2].regions, vec![Some(View), Some(View)], "{shared}");
    assert!(d.iter().all(|d| d.unmet.is_empty()), "{d:?}");
    let config = region_shared::region_config(&shared.consumers, StyxConfig::default());
    let native = config.backends.native;
    assert_eq!(native.crop, None);
    assert_eq!(native.second_output_region(), Some(0));
    assert_eq!(native.regions[0].unwrap().rect, Some(rect(0)));
    assert_eq!(native.pass_regions().collect::<Vec<_>>(), [1]);
}

/// More regions than the capture has slots: NV12 regions beyond them are unmet (strict
/// requests refuse the plan), luma ones fall back to views.
#[test]
fn regions_beyond_the_slots_are_views_or_unmet() {
    let dev = pisp("pisp");
    let many = |x0: u32| Frames::nv12().regions((0..10).map(|i| rect(x0 + 16 * i)));
    let shared = plan_many_with(&dev, &[Frames::nv12(), many(0), many(400)], &registry()).unwrap();
    let slots: usize = shared
        .consumers
        .iter()
        .map(|p| p.region.slots().count())
        .sum();
    assert_eq!(slots, crate::capture_api::MAX_NATIVE_REGIONS);
    assert_eq!(shared.consumers[2].delivered().unmet, vec![Unmet::Roi]);
    assert!(
        plan_many_with(
            &dev,
            &[Frames::nv12(), many(0), many(400).strict()],
            &registry()
        )
        .is_err()
    );
    let gray = Frames::gray().regions((0..10).map(|i| rect(400 + 16 * i)));
    let shared = plan_many_with(&dev, &[Frames::nv12(), many(0), gray], &registry()).unwrap();
    let d = shared.consumers[2].delivered();
    assert!(d.unmet.is_empty(), "{:?}", d.unmet);
    assert!(d.regions.iter().all(|r| *r == Some(View)));
}

/// Without a PiSP the overview is box-filtered to the size asked for (not the whole frame).
#[test]
fn without_an_isp_the_overview_is_box_filtered() {
    let soft = pisp("software");
    let plan = plan_frames_with(
        &soft,
        &Frames::gray().roi(rect(0)).overview(320, 200),
        &registry(),
    )
    .unwrap();
    let d = plan.delivered();
    assert_eq!((d.overview, d.hardware_overview), (Some((320, 200)), false));
    assert!(d.unmet.is_empty(), "{:?}", d.unmet);
}

fn grey(w: u32, h: u32, ts: u64, crop: Option<FrameRect>, fill: u8) -> FrameLease {
    let stride = w as usize;
    let mut buf = BufferPool::with_limits(1, stride * h as usize, 1).lease();
    buf.resize(stride * h as usize);
    buf.as_mut_slice().fill(fill);
    let res = Resolution::new(w, h).unwrap();
    let mut meta = FrameMeta::new(MediaFormat::new(FourCc::GREY, res, ColorSpace::Unknown), ts);
    meta.crop = crop;
    FrameLease::single_plane(meta, buf, stride * h as usize, stride)
}

/// A consumer's frame from the capture's: its region 0 from its slot, its region 1 renumbered,
/// the overview kept, other consumers' regions and outputs dropped; stale crops skipped.
#[test]
fn consumers_get_their_own_regions_from_the_capture_frames() {
    let dev = pisp("pisp");
    let shared = plan_many_with(
        &dev,
        &[
            Frames::gray().roi(rect(0)).overview(320, 200),
            Frames::gray()
                .regions([rect(300), rect(600)])
                .overview(320, 200)
                .skip_stale_regions(),
        ],
        &registry(),
    )
    .unwrap();
    let plan = &shared.consumers[1];
    assert_eq!(
        plan.region.places,
        vec![Some(IspPlace::Slot(0)), Some(IspPlace::Slot(1))]
    );
    let capture = grey(128, 128, 7, Some(rect(0)), 1)
        .with_companion(
            CompanionKind::Region { index: 1 },
            grey(128, 128, 7, Some(rect(300)), 2),
        )
        .unwrap()
        .with_companion(
            CompanionKind::Region { index: 2 },
            grey(128, 128, 7, Some(rect(600)), 3),
        )
        .unwrap()
        .with_companion(CompanionKind::Overview, grey(320, 200, 7, None, 4))
        .unwrap();
    let frame = region_frames::consumer_frame(capture, plan).unwrap();
    assert_eq!(frame.meta().crop, Some(rect(300)));
    assert_eq!(frame.region(1).unwrap().meta().crop, Some(rect(600)));
    assert!(frame.region(2).is_none());
    assert!(frame.overview().is_some());
    let roi = RoiHandle::default();
    roi.set_regions(&[rect(300), rect(600)]);
    assert!(region_frames::fresh(&frame, plan, &roi));
    // Moved: the frame the ISP cut before is stale until one with the new crop comes.
    roi.set_region(1, Some(rect(610)));
    assert!(!region_frames::fresh(&frame, plan, &roi));
    // Inside the old crop (as the ISP rounds it): still current.
    roi.set_region(1, Some(FrameRect::new(601, 65, 100, 100)));
    assert!(region_frames::fresh(&frame, plan, &roi));
    // The first consumer: the main output (the frame itself), without the others' regions.
    let capture = grey(128, 128, 7, Some(rect(0)), 1)
        .with_companion(
            CompanionKind::Region { index: 1 },
            grey(128, 128, 7, Some(rect(300)), 2),
        )
        .unwrap();
    let frame = region_frames::consumer_frame(capture, &shared.consumers[0]).unwrap();
    assert_eq!(frame.meta().crop, Some(rect(0)));
    assert!(frame.region(1).is_none());
    // Without its region (not set, or no buffer for its pass this frame): skipped.
    assert!(region_frames::consumer_frame(grey(128, 128, 7, None, 1), plan).is_err());
}

/// The overview box filter: halvings, then an area resize, the mean kept.
#[test]
fn overviews_box_filter_to_their_size() {
    let pool = BufferPool::lazy(0, 4);
    let frame = grey(1280, 720, 1, None, 0);
    let o = region_frames::box_overview(&frame, (570, 320), &pool)
        .unwrap()
        .unwrap();
    let res = o.meta().format.resolution;
    assert_eq!((res.width.get(), res.height.get()), (570, 320));
    let quarter = region_frames::box_overview(&grey(1280, 800, 1, None, 90), (320, 200), &pool)
        .unwrap()
        .unwrap();
    let rows = quarter.luma_rows().unwrap();
    assert_eq!((rows.row_bytes(), rows.len()), (320, 200));
    assert!(rows.row(100).unwrap().data().iter().all(|&v| v == 90));
    let odd = region_frames::box_overview(&grey(1280, 720, 1, None, 77), (570, 320), &pool)
        .unwrap()
        .unwrap();
    let rows = odd.luma_rows().unwrap();
    assert!(rows.row(319).unwrap().data().iter().all(|&v| v == 77));
}
