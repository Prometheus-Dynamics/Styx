use super::*;

#[test]
fn record_worker_error_keeps_last_failure_without_camera_hardware() {
    let worker_error = Mutex::new(None);
    let err = CaptureError::Backend("request loop failed".into());

    record_worker_error(&worker_error, &err);

    let stored = worker_error.lock().clone();
    assert_eq!(
        stored.as_ref().map(ToString::to_string),
        Some(err.to_string())
    );
}

/// libcamera names formats by DRM fourcc: DRM `XR24` (`XRGB8888`, bytes B, G, R, x) is Styx
/// (V4L2) `XR24`, DRM `RG24` (`RGB888`, bytes B, G, R) is Styx `BG24`, and so on.
#[test]
fn libcamera_formats_map_by_memory_layout() {
    use libcamera::pixel_format::PixelFormat;
    let lc = |code: &[u8; 4]| PixelFormat::new(u32::from_le_bytes(*code), 0);
    for (drm, styx) in [
        (b"XR24", FourCc::XR24),
        (b"XB24", FourCc::XB24),
        (b"AR24", FourCc::BGRA),
        (b"AB24", FourCc::RGBA),
        (b"RG24", FourCc::BG24),
        (b"BG24", FourCc::RG24),
        (b"NV12", FourCc::NV12),
        (b"YUYV", FourCc::YUYV),
    ] {
        assert_eq!(map_pixel_format_to_fourcc(lc(drm)), styx, "{styx}");
        assert_eq!(
            normalize_requested_fourcc_for_libcamera(styx),
            FourCc::new(*drm),
            "{styx}"
        );
    }
    assert_eq!(
        normalize_requested_fourcc_for_libcamera(FourCc::RGB3),
        FourCc::new(*b"BG24")
    );
    // Raw formats keep their Styx code in requests.
    let pbaa = FourCc::new(*b"pBAA");
    assert_eq!(normalize_requested_fourcc_for_libcamera(pbaa), pbaa);
    assert!(util::is_rgb24_request(FourCc::RG24) && util::is_rgb24_request(FourCc::BGR3));
    assert!(!util::is_rgb24_request(FourCc::XR24));
}
