#[cfg(feature = "v4l2")]
use super::*;
#[cfg(feature = "v4l2")]
use crate::ProbeResult;
#[cfg(feature = "v4l2")]
use crate::{BackendHandle, BackendKind, DeviceIdentity, ProbedBackend, ProbedDevice};
#[cfg(feature = "v4l2")]
use std::sync::{Arc, Mutex};
#[cfg(feature = "v4l2")]
use std::time::Duration;
#[cfg(feature = "v4l2")]
use styx_capture::{CaptureDescriptor, Mode, ModeId};
#[cfg(feature = "v4l2")]
use styx_core::prelude::{
    Access, ColorSpace, ControlId, ControlKind, ControlMeta, ControlMetadata, ControlValue, FourCc,
    MediaFormat, Resolution,
};

#[cfg(feature = "v4l2")]
fn device(id: &str, keys: &[&str], path: &str) -> ProbedDevice {
    ProbedDevice {
        identity: DeviceIdentity {
            display: id.to_string(),
            keys: keys.iter().map(|key| (*key).to_string()).collect(),
        },
        backends: vec![ProbedBackend {
            kind: BackendKind::V4l2,
            handle: BackendHandle::V4l2 {
                path: path.to_string(),
            },
            descriptor: CaptureDescriptor {
                modes: Vec::new(),
                controls: Vec::new(),
            },
            properties: vec![("path".into(), path.into())],
        }],
    }
}

#[test]
#[cfg(feature = "v4l2")]
fn diff_devices_reports_add_remove_and_change() {
    let previous = vec![device("cam-a", &["a"], "/dev/video0")];
    let next = vec![
        device("cam-a", &["a"], "/dev/video1"),
        device("cam-b", &["b"], "/dev/video2"),
    ];

    let diff = runtime::diff_devices(&previous, &next);
    assert_eq!(diff.added.len(), 1);
    assert_eq!(diff.removed.len(), 0);
    assert_eq!(diff.changed.len(), 1);
}

#[test]
#[cfg(feature = "v4l2")]
fn diff_devices_ignores_key_and_property_order() {
    let previous = vec![ProbedDevice {
        identity: DeviceIdentity {
            display: "cam-a".into(),
            keys: vec!["b".into(), "a".into()],
        },
        backends: vec![ProbedBackend {
            kind: BackendKind::V4l2,
            handle: BackendHandle::V4l2 {
                path: "/dev/video0".into(),
            },
            descriptor: CaptureDescriptor {
                modes: Vec::new(),
                controls: Vec::new(),
            },
            properties: vec![
                ("model".into(), "cam-a".into()),
                ("path".into(), "/dev/video0".into()),
            ],
        }],
    }];
    let next = vec![ProbedDevice {
        identity: DeviceIdentity {
            display: "cam-a".into(),
            keys: vec!["a".into(), "b".into()],
        },
        backends: vec![ProbedBackend {
            kind: BackendKind::V4l2,
            handle: BackendHandle::V4l2 {
                path: "/dev/video0".into(),
            },
            descriptor: CaptureDescriptor {
                modes: Vec::new(),
                controls: Vec::new(),
            },
            properties: vec![
                ("path".into(), "/dev/video0".into()),
                ("model".into(), "cam-a".into()),
            ],
        }],
    }];

    let diff = runtime::diff_devices(&previous, &next);
    assert!(diff.is_empty());
}

#[test]
#[cfg(feature = "v4l2")]
fn normalize_probe_result_sorts_backend_descriptor_content() {
    let mut probe = ProbeResult {
        devices: vec![ProbedDevice {
            identity: DeviceIdentity {
                display: "cam-a".into(),
                keys: vec!["b".into(), "a".into(), "a".into()],
            },
            backends: vec![ProbedBackend {
                kind: BackendKind::V4l2,
                handle: BackendHandle::V4l2 {
                    path: "/dev/video0".into(),
                },
                descriptor: CaptureDescriptor {
                    modes: vec![
                        Mode {
                            id: ModeId {
                                format: MediaFormat::new(
                                    FourCc::new(*b"RG24"),
                                    Resolution::new(1280, 720).expect("res"),
                                    ColorSpace::Srgb,
                                ),
                                interval: None,
                            },
                            format: MediaFormat::new(
                                FourCc::new(*b"RG24"),
                                Resolution::new(1280, 720).expect("res"),
                                ColorSpace::Srgb,
                            ),
                            intervals: smallvec::smallvec![],
                            interval_stepwise: None,
                        },
                        Mode {
                            id: ModeId {
                                format: MediaFormat::new(
                                    FourCc::new(*b"RG24"),
                                    Resolution::new(640, 480).expect("res"),
                                    ColorSpace::Srgb,
                                ),
                                interval: None,
                            },
                            format: MediaFormat::new(
                                FourCc::new(*b"RG24"),
                                Resolution::new(640, 480).expect("res"),
                                ColorSpace::Srgb,
                            ),
                            intervals: smallvec::smallvec![],
                            interval_stepwise: None,
                        },
                    ],
                    controls: vec![
                        ControlMeta {
                            id: ControlId(2),
                            name: "b".into(),
                            kind: ControlKind::Uint,
                            access: Access::ReadWrite,
                            min: ControlValue::Uint(0),
                            max: ControlValue::Uint(1),
                            default: ControlValue::Uint(0),
                            step: None,
                            menu: None,
                            metadata: ControlMetadata::default(),
                        },
                        ControlMeta {
                            id: ControlId(1),
                            name: "a".into(),
                            kind: ControlKind::Uint,
                            access: Access::ReadWrite,
                            min: ControlValue::Uint(0),
                            max: ControlValue::Uint(1),
                            default: ControlValue::Uint(0),
                            step: None,
                            menu: None,
                            metadata: ControlMetadata::default(),
                        },
                    ],
                },
                properties: vec![
                    ("path".into(), "/dev/video0".into()),
                    ("model".into(), "cam-a".into()),
                    ("model".into(), "cam-a".into()),
                ],
            }],
        }],
        errors: Vec::new(),
    };

    probe = runtime::normalize_probe_result(probe);
    let device = &probe.devices[0];
    assert_eq!(device.identity.keys, vec!["a", "b"]);
    assert_eq!(
        device.backends[0].properties,
        vec![
            ("model".into(), "cam-a".into()),
            ("path".into(), "/dev/video0".into()),
        ]
    );
    assert_eq!(device.backends[0].descriptor.controls[0].id.0, 1);
    assert_eq!(
        device.backends[0].descriptor.modes[0]
            .format
            .resolution
            .width
            .get(),
        640
    );
}

#[test]
#[cfg(feature = "v4l2")]
fn merge_probe_result_retains_untouched_backends() {
    let current = ProbeResult {
        devices: vec![ProbedDevice {
            identity: DeviceIdentity {
                display: "cam".into(),
                keys: vec!["shared".into()],
            },
            backends: vec![
                ProbedBackend {
                    kind: BackendKind::V4l2,
                    handle: BackendHandle::V4l2 {
                        path: "/dev/video0".into(),
                    },
                    descriptor: CaptureDescriptor {
                        modes: Vec::new(),
                        controls: Vec::new(),
                    },
                    properties: vec![("path".into(), "/dev/video0".into())],
                },
                #[cfg(feature = "libcamera")]
                ProbedBackend {
                    kind: BackendKind::Libcamera,
                    handle: BackendHandle::Libcamera { id: "cam0".into() },
                    descriptor: CaptureDescriptor {
                        modes: Vec::new(),
                        controls: Vec::new(),
                    },
                    properties: vec![("model".into(), "cam".into())],
                },
            ],
        }],
        errors: vec![
            "v4l2: old".into(),
            #[cfg(feature = "libcamera")]
            "libcamera: keep".into(),
        ],
    };
    let partial = ProbeResult {
        devices: vec![device("cam", &["shared"], "/dev/video1")],
        errors: Vec::new(),
    };
    let merged = runtime::merge_probe_result(&current, partial, &[BackendKind::V4l2]);
    assert!(merged.devices.iter().any(|device| device
        .backends
        .iter()
        .any(|backend| matches!(&backend.handle, BackendHandle::V4l2 { path } if path == "/dev/video1"))));
    #[cfg(feature = "libcamera")]
    assert!(merged.devices.iter().any(|device| {
        device
            .backends
            .iter()
            .any(|backend| matches!(backend.kind, BackendKind::Libcamera))
    }));
    #[cfg(feature = "libcamera")]
    assert!(merged.errors.iter().any(|error| error == "libcamera: keep"));
}

#[test]
#[cfg(feature = "v4l2")]
fn merge_probe_result_retains_failing_backend_inventory() {
    let current = ProbeResult {
        devices: vec![device("cam", &["shared"], "/dev/video0")],
        errors: Vec::new(),
    };
    let partial = ProbeResult {
        devices: Vec::new(),
        errors: vec!["v4l2: transient probe failure".into()],
    };
    let merged = runtime::merge_probe_result(&current, partial, &[BackendKind::V4l2]);
    assert_eq!(merged.devices.len(), 1);
    assert!(merged.devices[0].backends.iter().any(
        |backend| matches!(&backend.handle, BackendHandle::V4l2 { path } if path == "/dev/video0")
    ));
    assert!(
        merged
            .errors
            .iter()
            .any(|error| error == "v4l2: transient probe failure")
    );
}

#[cfg(all(feature = "async", feature = "v4l2"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn async_subscription_observes_retained_events() {
    let mut runtime = WatchRuntime::new();
    let next = ProbeResult {
        devices: vec![device("cam-a", &["a"], "/dev/video0")],
        errors: Vec::new(),
    };
    let _ = runtime.finish_refresh(next, Vec::new());
    let async_runtime = AsyncWatchRuntime::new(runtime);
    let subscription = async_runtime.subscribe_from_start();
    let events = subscription.poll().await.expect("poll events");
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], InventoryEvent::Added(_)));
}

#[test]
#[cfg(feature = "v4l2")]
fn blocking_subscription_waits_for_new_events() {
    let runtime = Arc::new(Mutex::new(WatchRuntime::new()));
    let mut subscription = runtime
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .subscribe_from_start_blocking();

    let producer_runtime = Arc::clone(&runtime);
    let producer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(25));
        let next = ProbeResult {
            devices: vec![device("cam-a", &["a"], "/dev/video0")],
            errors: Vec::new(),
        };
        producer_runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .finish_refresh(next, Vec::new());
    });

    assert!(subscription.wait_for_update(Some(Duration::from_secs(1))));
    let events = {
        let runtime = runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        subscription.poll(&runtime).to_vec()
    };
    producer.join().expect("producer thread");

    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], InventoryEvent::Added(_)));
}

#[test]
#[cfg(feature = "v4l2")]
fn finish_scoped_refresh_records_added_event() {
    let mut runtime = WatchRuntime::new();
    let report = runtime.finish_scoped_refresh(
        ProbeResult {
            devices: vec![device("cam-a", &["a"], "/dev/video0")],
            errors: Vec::new(),
        },
        &[BackendKind::V4l2],
        vec![DeviceWatchEvent::new(
            "test-watcher",
            vec![BackendKind::V4l2],
            vec!["/dev/video0".into()],
        )],
    );

    assert_eq!(report.watch_events.len(), 1);
    assert_eq!(report.diff.added.len(), 1);
    assert_eq!(runtime.events().len(), 1);
    assert!(matches!(runtime.events()[0], InventoryEvent::Added(_)));
}

#[test]
#[cfg(feature = "v4l2")]
fn finish_scoped_refresh_ignores_degraded_backend_removal() {
    let mut runtime = WatchRuntime::new();
    runtime.finish_refresh(
        ProbeResult {
            devices: vec![device("cam-a", &["a"], "/dev/video0")],
            errors: Vec::new(),
        },
        Vec::new(),
    );

    let report = runtime.finish_scoped_refresh(
        ProbeResult {
            devices: Vec::new(),
            errors: vec!["v4l2: transient probe failure".into()],
        },
        &[BackendKind::V4l2],
        vec![DeviceWatchEvent::new(
            "test-watcher",
            vec![BackendKind::V4l2],
            vec!["/dev/video0".into()],
        )],
    );

    assert!(report.diff.is_empty());
    assert_eq!(runtime.devices().len(), 1);
}

#[test]
#[cfg(all(feature = "v4l2", feature = "libcamera"))]
fn finish_scoped_refresh_ignores_degraded_libcamera_removal() {
    let mut runtime = WatchRuntime::new();
    runtime.finish_refresh(
        ProbeResult {
            devices: vec![ProbedDevice {
                identity: DeviceIdentity {
                    display: "cam-a".into(),
                    keys: vec!["a".into(), "shared".into()],
                },
                backends: vec![
                    ProbedBackend {
                        kind: BackendKind::V4l2,
                        handle: BackendHandle::V4l2 {
                            path: "/dev/video0".into(),
                        },
                        descriptor: CaptureDescriptor {
                            modes: Vec::new(),
                            controls: Vec::new(),
                        },
                        properties: vec![("path".into(), "/dev/video0".into())],
                    },
                    ProbedBackend {
                        kind: BackendKind::Libcamera,
                        handle: BackendHandle::Libcamera { id: "cam0".into() },
                        descriptor: CaptureDescriptor {
                            modes: Vec::new(),
                            controls: Vec::new(),
                        },
                        properties: vec![("model".into(), "cam-a".into())],
                    },
                ],
            }],
            errors: Vec::new(),
        },
        Vec::new(),
    );

    let report = runtime.finish_scoped_refresh(
        ProbeResult {
            devices: Vec::new(),
            errors: vec!["libcamera: transient probe failure".into()],
        },
        &[BackendKind::Libcamera],
        vec![DeviceWatchEvent::new(
            "test-watcher",
            vec![BackendKind::Libcamera],
            vec!["cam0".into()],
        )],
    );

    assert!(report.diff.is_empty());
    assert_eq!(runtime.devices().len(), 1);
    assert_eq!(runtime.devices()[0].backends.len(), 2);
}

#[test]
#[cfg(feature = "v4l2")]
fn watcher_refresh_waits_for_settle_window() {
    struct StaticWatcher {
        events: Vec<DeviceWatchEvent>,
    }

    impl DeviceWatcher for StaticWatcher {
        fn name(&self) -> &'static str {
            "static-watcher"
        }

        fn poll(&mut self) -> Result<Vec<DeviceWatchEvent>, WatchError> {
            Ok(std::mem::take(&mut self.events))
        }
    }

    let mut runtime = WatchRuntime::with_config(WatchRuntimeConfig {
        watch_settle_time: Duration::from_millis(50),
        ..WatchRuntimeConfig::default()
    });
    let mut watcher = StaticWatcher {
        events: vec![DeviceWatchEvent::new(
            "static-watcher",
            vec![BackendKind::V4l2],
            vec!["/dev/video0".into()],
        )],
    };

    let first = runtime
        .poll_watcher_and_refresh(&mut watcher)
        .expect("first poll");
    assert!(first.is_none());

    std::thread::sleep(Duration::from_millis(60));
    let second = runtime
        .poll_watcher_and_refresh(&mut watcher)
        .expect("second poll");
    assert!(second.is_some());
}
