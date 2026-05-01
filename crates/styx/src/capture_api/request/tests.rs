use super::*;
use crate::BackendHandle;

#[test]
fn pick_mode_ignores_color_when_unknown() {
    let fmt_advertised = MediaFormat::new(
        FourCc::new(*b"RGGB"),
        Resolution::new(1280, 800).unwrap(),
        ColorSpace::Unknown,
    );
    let fmt_requested = MediaFormat::new(
        FourCc::new(*b"RGGB"),
        Resolution::new(1280, 800).unwrap(),
        ColorSpace::Bt709,
    );
    let advertised_mode = Mode {
        id: ModeId {
            format: fmt_advertised,
            interval: None,
        },
        format: fmt_advertised,
        intervals: smallvec::smallvec![],
        interval_stepwise: None,
    };
    let backend = ProbedBackend {
        kind: BackendKind::Virtual,
        handle: BackendHandle::Virtual,
        descriptor: CaptureDescriptor {
            modes: vec![advertised_mode.clone()],
            controls: vec![],
        },
        properties: vec![],
    };

    let requested_id = ModeId {
        format: fmt_requested,
        interval: None,
    };
    let picked = pick_mode(&backend, Some(requested_id)).expect("pick");
    assert_eq!(picked.id.format.code, FourCc::new(*b"RGGB"));
    assert_eq!(picked.id.format.resolution.width.get(), 1280);
    assert_eq!(picked.id.format.resolution.height.get(), 800);
}

#[test]
fn pick_mode_accepts_mode_format_when_id_format_differs() {
    let fmt_id = MediaFormat::new(
        FourCc::new(*b"RGGB"),
        Resolution::new(1280, 800).unwrap(),
        ColorSpace::Unknown,
    );
    let fmt_mode = MediaFormat::new(
        FourCc::new(*b"RGGB"),
        Resolution::new(1280, 800).unwrap(),
        ColorSpace::Srgb,
    );
    let advertised_mode = Mode {
        id: ModeId {
            format: fmt_id,
            interval: None,
        },
        format: fmt_mode,
        intervals: smallvec::smallvec![],
        interval_stepwise: None,
    };
    let backend = ProbedBackend {
        kind: BackendKind::Virtual,
        handle: BackendHandle::Virtual,
        descriptor: CaptureDescriptor {
            modes: vec![advertised_mode.clone()],
            controls: vec![],
        },
        properties: vec![],
    };

    let requested = ModeId {
        format: fmt_mode,
        interval: None,
    };
    let picked = pick_mode(&backend, Some(requested)).expect("pick");
    assert_eq!(picked.format.color, ColorSpace::Srgb);
}

#[test]
fn pick_mode_relaxes_color_for_bayer() {
    let fmt_advertised = MediaFormat::new(
        FourCc::new(*b"RGGB"),
        Resolution::new(1280, 800).unwrap(),
        ColorSpace::Bt709,
    );
    let fmt_requested = MediaFormat::new(
        FourCc::new(*b"RGGB"),
        Resolution::new(1280, 800).unwrap(),
        ColorSpace::Srgb,
    );
    let advertised_mode = Mode {
        id: ModeId {
            format: fmt_advertised,
            interval: None,
        },
        format: fmt_advertised,
        intervals: smallvec::smallvec![],
        interval_stepwise: None,
    };
    let backend = ProbedBackend {
        kind: BackendKind::Virtual,
        handle: BackendHandle::Virtual,
        descriptor: CaptureDescriptor {
            modes: vec![advertised_mode.clone()],
            controls: vec![],
        },
        properties: vec![],
    };

    let requested_id = ModeId {
        format: fmt_requested,
        interval: None,
    };
    let picked = pick_mode(&backend, Some(requested_id)).expect("pick");
    assert_eq!(picked.id.format.code, FourCc::new(*b"RGGB"));
}

#[test]
fn resolved_descriptor_returns_only_selected_mode() {
    let fmt_primary = MediaFormat::new(
        FourCc::RG24,
        Resolution::new(640, 480).unwrap(),
        ColorSpace::Srgb,
    );
    let fmt_secondary = MediaFormat::new(
        FourCc::RG24,
        Resolution::new(1280, 720).unwrap(),
        ColorSpace::Srgb,
    );
    let requested_mode = Mode {
        id: ModeId {
            format: fmt_secondary,
            interval: None,
        },
        format: fmt_secondary,
        intervals: smallvec::smallvec![],
        interval_stepwise: None,
    };
    let backend = ProbedBackend {
        kind: BackendKind::Virtual,
        handle: BackendHandle::Virtual,
        descriptor: CaptureDescriptor {
            modes: vec![
                Mode {
                    id: ModeId {
                        format: fmt_primary,
                        interval: None,
                    },
                    format: fmt_primary,
                    intervals: smallvec::smallvec![],
                    interval_stepwise: None,
                },
                requested_mode.clone(),
            ],
            controls: vec![],
        },
        properties: vec![],
    };
    let device = ProbedDevice {
        identity: crate::DeviceIdentity {
            display: "virtual".to_string(),
            keys: vec!["virtual".to_string()],
        },
        backends: vec![backend],
    };

    let descriptor = CaptureRequest::new(&device)
        .backend(BackendKind::Virtual)
        .mode(requested_mode.id.clone())
        .resolved_descriptor()
        .expect("resolve descriptor");

    assert_eq!(descriptor.modes.len(), 1);
    assert_eq!(descriptor.modes[0].id, requested_mode.id);
}

#[test]
fn camera_request_selects_best_matching_mode() {
    let small = MediaFormat::new(
        FourCc::RG24,
        Resolution::new(640, 480).unwrap(),
        ColorSpace::Srgb,
    );
    let large = MediaFormat::new(
        FourCc::RG24,
        Resolution::new(1920, 1080).unwrap(),
        ColorSpace::Srgb,
    );
    let slow = Interval {
        numerator: std::num::NonZeroU32::new(1).unwrap(),
        denominator: std::num::NonZeroU32::new(30).unwrap(),
    };
    let fast = Interval {
        numerator: std::num::NonZeroU32::new(1).unwrap(),
        denominator: std::num::NonZeroU32::new(60).unwrap(),
    };
    let modes = vec![
        Mode {
            id: ModeId {
                format: large,
                interval: Some(slow),
            },
            format: large,
            intervals: smallvec::smallvec![slow],
            interval_stepwise: None,
        },
        Mode {
            id: ModeId {
                format: small,
                interval: Some(fast),
            },
            format: small,
            intervals: smallvec::smallvec![slow, fast],
            interval_stepwise: None,
        },
    ];
    let device = ProbedDevice {
        identity: crate::DeviceIdentity {
            display: "virtual".to_string(),
            keys: vec!["virtual".to_string()],
        },
        backends: vec![ProbedBackend {
            kind: BackendKind::Virtual,
            handle: BackendHandle::Virtual,
            descriptor: CaptureDescriptor {
                modes,
                controls: vec![],
            },
            properties: vec![],
        }],
    };

    let selected = CameraRequest::from_devices(vec![device])
        .max_resolution(640, 480)
        .select()
        .expect("camera selection");

    assert_eq!(selected.backend, BackendKind::Virtual);
    assert_eq!(selected.mode.format.resolution, small.resolution);
    assert_eq!(selected.interval, Some(fast));
}

#[test]
fn camera_request_uses_backend_priority() {
    let fmt = MediaFormat::new(
        FourCc::RG24,
        Resolution::new(640, 480).unwrap(),
        ColorSpace::Srgb,
    );
    let mode = Mode {
        id: ModeId {
            format: fmt,
            interval: None,
        },
        format: fmt,
        intervals: smallvec::smallvec![],
        interval_stepwise: None,
    };
    let device = ProbedDevice {
        identity: crate::DeviceIdentity {
            display: "dual".to_string(),
            keys: vec!["dual".to_string()],
        },
        backends: vec![
            ProbedBackend {
                kind: BackendKind::Libcamera,
                handle: BackendHandle::Virtual,
                descriptor: CaptureDescriptor {
                    modes: vec![mode.clone()],
                    controls: vec![],
                },
                properties: vec![],
            },
            ProbedBackend {
                kind: BackendKind::V4l2,
                handle: BackendHandle::Virtual,
                descriptor: CaptureDescriptor {
                    modes: vec![mode],
                    controls: vec![],
                },
                properties: vec![],
            },
        ],
    };

    let selected = CameraRequest::from_devices(vec![device])
        .backend_priority([BackendKind::Libcamera, BackendKind::V4l2])
        .select()
        .expect("camera selection");

    assert_eq!(selected.backend, BackendKind::Libcamera);
}

#[test]
fn camera_request_accepts_string_format_priority_and_selects_all() {
    let rg24 = MediaFormat::new(
        FourCc::RG24,
        Resolution::new(640, 480).unwrap(),
        ColorSpace::Srgb,
    );
    let nv12 = MediaFormat::new(
        FourCc::NV12,
        Resolution::new(640, 480).unwrap(),
        ColorSpace::Srgb,
    );
    let make_backend = |kind, format| ProbedBackend {
        kind,
        handle: BackendHandle::Virtual,
        descriptor: CaptureDescriptor {
            modes: vec![Mode {
                id: ModeId {
                    format,
                    interval: None,
                },
                format,
                intervals: smallvec::smallvec![],
                interval_stepwise: None,
            }],
            controls: vec![],
        },
        properties: vec![],
    };
    let first_device = ProbedDevice {
        identity: crate::DeviceIdentity {
            display: "formats-a".to_string(),
            keys: vec!["formats-a".to_string()],
        },
        backends: vec![
            make_backend(BackendKind::Virtual, rg24),
            make_backend(BackendKind::File, nv12),
        ],
    };
    let second_device = ProbedDevice {
        identity: crate::DeviceIdentity {
            display: "formats-b".to_string(),
            keys: vec!["formats-b".to_string()],
        },
        backends: vec![make_backend(BackendKind::Virtual, rg24)],
    };

    let selected = CameraRequest::from_devices(vec![first_device, second_device])
        .backend_priority([BackendKind::Virtual, BackendKind::File])
        .try_format_priority(["NV12", "RG24"])
        .expect("valid format priority")
        .min_resolution(320, 240)
        .select_many(2)
        .expect("camera selection");

    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0].mode.format.code, FourCc::NV12);
}

#[test]
fn camera_request_rejects_invalid_string_format_priority_without_panicking() {
    let err = CameraRequest::from_devices(Vec::new())
        .try_format_priority(["RGB"])
        .expect_err("invalid fourcc should be rejected");

    assert!(matches!(err, CaptureError::InvalidConfig(_)));
}

#[test]
fn camera_request_filters_and_ranks_resolution_priority() {
    let small = MediaFormat::new(
        FourCc::RG24,
        Resolution::new(640, 480).unwrap(),
        ColorSpace::Srgb,
    );
    let preferred = MediaFormat::new(
        FourCc::RG24,
        Resolution::new(1280, 720).unwrap(),
        ColorSpace::Srgb,
    );
    let fallback = MediaFormat::new(
        FourCc::RG24,
        Resolution::new(1920, 1080).unwrap(),
        ColorSpace::Srgb,
    );
    let modes = [small, fallback, preferred]
        .into_iter()
        .map(|format| Mode {
            id: ModeId {
                format,
                interval: None,
            },
            format,
            intervals: smallvec::smallvec![],
            interval_stepwise: None,
        })
        .collect();
    let device = ProbedDevice {
        identity: crate::DeviceIdentity {
            display: "resolutions".to_string(),
            keys: vec!["resolutions".to_string()],
        },
        backends: vec![ProbedBackend {
            kind: BackendKind::Virtual,
            handle: BackendHandle::Virtual,
            descriptor: CaptureDescriptor {
                modes,
                controls: vec![],
            },
            properties: vec![],
        }],
    };

    let selected = CameraRequest::from_devices(vec![device])
        .resolution_priority([(1280, 720), (1920, 1080)])
        .select()
        .expect("camera selection");

    assert_eq!(selected.mode.format.resolution, preferred.resolution);
}

#[test]
fn capture_request_config_overrides_queue_depth_per_request() {
    let device = crate::capture_api::make_virtual_rgb_device("local-config", 2, 2, 30);
    let handle = CaptureRequest::new(&device)
        .backend(BackendKind::Virtual)
        .config(StyxConfig::new().capture_queue_depth(2))
        .start()
        .expect("start capture with local config");

    assert_eq!(handle.queue_stats().capacity, 2);
    handle.stop();
}

#[test]
fn capture_source_opens_with_config_without_manual_request_builder() {
    let device = crate::capture_api::make_virtual_rgb_device("source-open-config", 2, 2, 30);
    let source = CaptureSource::new(device);
    let handle = source
        .open_with_config(StyxConfig::new().capture_queue_depth(2))
        .expect("open capture with local config");

    assert_eq!(handle.queue_stats().capacity, 2);
    handle.stop();
}

#[test]
fn capture_source_builds_pipeline_without_manual_request_builder() {
    let device = crate::capture_api::make_virtual_rgb_device("source-pipeline", 2, 2, 30);
    let source = CaptureSource::new(device);
    let mut pipeline = source
        .pipeline()
        .raw_frames()
        .start()
        .expect("start pipeline from source");

    assert!(matches!(
        pipeline.next_blocking(std::time::Duration::from_millis(250)),
        RecvOutcome::Data(_)
    ));
    pipeline.stop();
}

#[cfg(feature = "async")]
#[test]
fn capture_request_async_policy_start_uses_request_config() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let device = crate::capture_api::make_virtual_rgb_device("async-local-config", 2, 2, 30);
        let handle = CaptureRequest::new(&device)
            .backend(BackendKind::Virtual)
            .config(StyxConfig::new().capture_queue_depth(2))
            .start_with_policy_async(CaptureStartPolicy::default())
            .await
            .expect("start capture with async policy");

        assert_eq!(handle.queue_stats().capacity, 2);
        handle.stop();
    });
}

#[test]
fn camera_request_config_flows_to_started_capture() {
    let device = crate::capture_api::make_virtual_rgb_device("camera-local-config", 2, 2, 30);
    let handle = CameraRequest::from_devices(vec![device])
        .backend_priority([BackendKind::Virtual])
        .config(StyxConfig::new().capture_queue_depth(2))
        .start()
        .expect("start camera with local config");

    assert_eq!(handle.queue_stats().capacity, 2);
    handle.stop();
}

#[cfg(feature = "libcamera")]
#[test]
fn tdn_retry_disables_output_even_when_noise_reduction_is_already_off() {
    let device = crate::capture_api::make_virtual_rgb_device("tdn-retry", 2, 2, 30);
    let mut request = CaptureRequest::new(&device)
        .tdn_output_mode(TdnOutputMode::Force)
        .control(LIBCAMERA_NOISE_REDUCTION_MODE, ControlValue::Int(0));

    assert!(request.try_disable_noise_reduction(
        BackendKind::Libcamera,
        &CaptureError::LibcameraTdnOutputUnavailable,
    ));
    assert_eq!(request.tdn_output_mode, TdnOutputMode::Off);
    assert_eq!(
        request.controls,
        vec![(LIBCAMERA_NOISE_REDUCTION_MODE, ControlValue::Int(0))]
    );
}
