use super::{
    build_v4l2_single_plane_layout, plan_v4l2_single_plane_layout, supports_v4l2_mmap_zero_copy,
};
use styx_core::prelude::FourCc;

#[test]
fn encoded_layout_uses_bytes_used() {
    let layout = build_v4l2_single_plane_layout(true, 1080, 0, 4096).expect("layout");
    assert_eq!(layout.len, 4096);
    assert_eq!(layout.stride, 4096);
}

#[test]
fn raw_layout_uses_stride_times_height() {
    let layout = build_v4l2_single_plane_layout(false, 2, 6, 12).expect("layout");
    assert_eq!(layout.len, 12);
    assert_eq!(layout.stride, 6);
}

#[test]
fn raw_layout_rejects_short_buffer() {
    assert!(build_v4l2_single_plane_layout(false, 2, 6, 10).is_none());
}

#[test]
fn zero_copy_whitelist_accepts_initial_validated_formats() {
    for code in [
        FourCc::MJPG,
        FourCc::JPEG,
        FourCc::YUYV,
        FourCc::RG24,
        FourCc::new(*b"RGB3"),
        FourCc::BGR3,
        FourCc::RGBA,
        FourCc::BGRA,
    ] {
        assert!(
            supports_v4l2_mmap_zero_copy(code),
            "expected {code} to be whitelisted"
        );
    }
}

#[test]
fn zero_copy_whitelist_rejects_deferred_formats() {
    for code in [FourCc::NV12, FourCc::H264, FourCc::new(*b"BA81")] {
        assert!(
            !supports_v4l2_mmap_zero_copy(code),
            "expected {code} to use fallback"
        );
    }
}

#[test]
fn layout_plan_uses_negotiated_stride() {
    let plan =
        plan_v4l2_single_plane_layout(FourCc::YUYV, 640, 480, 1408, 675_840, 675_840, 675_840)
            .expect("layout plan");
    assert_eq!(plan.layout.stride, 1408);
    assert_eq!(plan.layout.len, 675_840);
    assert!(plan.zero_copy_safe);
}

#[test]
fn layout_plan_rejects_short_raw_buffers() {
    assert!(
        plan_v4l2_single_plane_layout(FourCc::YUYV, 640, 480, 1280, 614_400, 614_400, 614_399,)
            .is_none()
    );
}

#[test]
fn layout_plan_marks_oversized_layout_unsafe_for_zero_copy() {
    let plan = plan_v4l2_single_plane_layout(FourCc::YUYV, 640, 480, 1280, 1, 614_400, 614_400)
        .expect("layout plan");
    assert!(!plan.zero_copy_safe);
}

#[test]
fn encoded_layout_plan_uses_bytes_used() {
    let plan =
        plan_v4l2_single_plane_layout(FourCc::MJPG, 1920, 1080, 0, 2_000_000, 2_000_000, 123_456)
            .expect("layout plan");
    assert_eq!(plan.layout.len, 123_456);
    assert_eq!(plan.layout.stride, 123_456);
    assert!(plan.zero_copy_safe);
}
