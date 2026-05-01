use std::time::{Duration, Instant};

use super::handle::*;
use super::*;
use crate::metrics::StageMetrics;

#[test]
fn health_report_includes_capture_worker_error() {
    let (_tx, rx) = styx_core::queue::bounded(1);
    let worker_error = std::sync::Arc::new(parking_lot::Mutex::new(Some(CaptureError::Backend(
        "worker failed".to_string(),
    ))));
    let handle = CaptureHandle {
        backend: BackendKind::Virtual,
        control: ControlPlane::Virtual,
        descriptor: CaptureDescriptor::new([]),
        mode: Mode::new(MediaFormat::srgb(FourCc::RG24, 1, 1).expect("test format")),
        interval: None,
        rx,
        stop_tx: None,
        worker: None,
        aux_workers: Vec::new(),
        #[cfg(feature = "libcamera")]
        libcamera_idle_stop_allowed: false,
        #[cfg(feature = "libcamera")]
        libcamera_stop_when_idle: false,
        metrics: StageMetrics::default(),
        external_backings: Vec::new(),
        worker_error,
        control_error: std::sync::Arc::new(parking_lot::Mutex::new(None)),
    };

    let report = handle.health_report();
    assert_eq!(report.recent_stage_errors.len(), 1);
    assert_eq!(report.recent_stage_errors[0].component, "virtual");
    assert!(
        report.recent_stage_errors[0]
            .message
            .contains("worker failed")
    );
}

#[test]
fn health_report_includes_control_error() {
    let (_tx, rx) = styx_core::queue::bounded(1);
    let control_error = std::sync::Arc::new(parking_lot::Mutex::new(Some(
        CaptureError::ControlUnsupported,
    )));
    let handle = CaptureHandle {
        backend: BackendKind::Virtual,
        control: ControlPlane::Virtual,
        descriptor: CaptureDescriptor::new([]),
        mode: Mode::new(MediaFormat::srgb(FourCc::RG24, 1, 1).expect("test format")),
        interval: None,
        rx,
        stop_tx: None,
        worker: None,
        aux_workers: Vec::new(),
        #[cfg(feature = "libcamera")]
        libcamera_idle_stop_allowed: false,
        #[cfg(feature = "libcamera")]
        libcamera_stop_when_idle: false,
        metrics: StageMetrics::default(),
        external_backings: Vec::new(),
        worker_error: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        control_error,
    };

    let report = handle.health_report();
    assert_eq!(report.recent_stage_errors.len(), 1);
    assert_eq!(report.recent_stage_errors[0].component, "virtual.control");
    assert!(
        report.recent_stage_errors[0]
            .message
            .contains("control plane not available")
    );
}

#[test]
fn memory_stats_include_external_backing_telemetry() {
    let (_tx, rx) = styx_core::queue::bounded(1);
    let tracker = std::sync::Arc::new(crate::metrics::ExternalBackingTracker::new("test_dmabuf"));
    tracker.acquire_many(2, 4096);
    let handle = CaptureHandle {
        backend: BackendKind::Virtual,
        control: ControlPlane::Virtual,
        descriptor: CaptureDescriptor::new([]),
        mode: Mode::new(MediaFormat::srgb(FourCc::RG24, 1, 1).expect("test format")),
        interval: None,
        rx,
        stop_tx: None,
        worker: None,
        aux_workers: Vec::new(),
        #[cfg(feature = "libcamera")]
        libcamera_idle_stop_allowed: false,
        #[cfg(feature = "libcamera")]
        libcamera_stop_when_idle: false,
        metrics: StageMetrics::default(),
        external_backings: vec![tracker],
        worker_error: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        control_error: std::sync::Arc::new(parking_lot::Mutex::new(None)),
    };

    let memory = handle.memory_stats();
    assert_eq!(memory.external_backings.len(), 1);
    assert_eq!(memory.external_backings[0].label, "test_dmabuf");
    assert_eq!(memory.external_backings[0].current_buffers, 2);
    assert_eq!(memory.external_backings[0].current_bytes, 4096);

    let health = handle.health_report();
    assert_eq!(health.external_inflight_buffers, 2);
    assert_eq!(health.external_inflight_bytes, 4096);
}

#[cfg(feature = "libcamera")]
#[test]
fn libcamera_get_control_times_out_when_worker_does_not_reply() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let control = ControlPlane::Libcamera {
        tx,
        pending: std::sync::Arc::new(parking_lot::Mutex::new(Default::default())),
        response_timeout: Duration::from_millis(1),
    };

    let err = read_control_from_plane(&control, ControlId(1)).expect_err("timeout");
    assert!(err.to_string().contains("timed out"));
}

#[test]
fn virtual_capture_stop_is_prompt_without_consumer() {
    let handle = crate::capture_api::open_virtual_rgb("stop-test", 2, 2, 1).expect("virtual");

    let started = Instant::now();
    handle.stop();

    assert!(
        started.elapsed() < Duration::from_millis(250),
        "virtual stop took {:?}",
        started.elapsed()
    );
}

#[cfg(feature = "async")]
#[test]
fn virtual_capture_stop_async_is_prompt_without_consumer() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let handle =
            crate::capture_api::open_virtual_rgb("async-stop-test", 2, 2, 1).expect("virtual");

        let started = Instant::now();
        handle.stop_async().await;

        assert!(
            started.elapsed() < Duration::from_millis(250),
            "virtual async stop took {:?}",
            started.elapsed()
        );
    });
}

#[test]
fn virtual_capture_respects_requested_interval() {
    let device = crate::capture_api::make_virtual_rgb_device("pace-test", 2, 2, 5);
    let handle = crate::capture_api::CaptureRequest::new(&device)
        .backend(crate::BackendKind::Virtual)
        .start()
        .expect("virtual capture");

    assert!(matches!(
        handle.recv_blocking(Duration::from_millis(250)),
        RecvOutcome::Data(_)
    ));
    assert!(matches!(
        handle.recv_blocking(Duration::from_millis(50)),
        RecvOutcome::Empty
    ));

    handle.stop();
}

#[cfg(feature = "file-backend")]
#[test]
fn file_capture_stop_interrupts_frame_pacing_sleep() {
    let (dir, path) = write_temp_png("styx-file-stop");

    let device = crate::capture_api::make_file_device("file-stop", vec![path], 1, true);
    let handle = crate::capture_api::CaptureRequest::new(&device)
        .backend(crate::BackendKind::File)
        .start()
        .expect("file capture");
    let _ = handle.recv_blocking(Duration::from_millis(500));

    let started = Instant::now();
    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        started.elapsed() < Duration::from_millis(250),
        "file stop took {:?}",
        started.elapsed()
    );
}

#[cfg(all(feature = "file-backend", feature = "async"))]
#[test]
fn file_capture_stop_async_interrupts_frame_pacing_sleep() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (dir, path) = write_temp_png("styx-file-stop-async");

        let device = crate::capture_api::make_file_device("file-stop-async", vec![path], 1, true);
        let handle = crate::capture_api::CaptureRequest::new(&device)
            .backend(crate::BackendKind::File)
            .start()
            .expect("file capture");
        let _ = handle.recv_blocking(Duration::from_millis(500));

        let started = Instant::now();
        handle.stop_async().await;
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            started.elapsed() < Duration::from_millis(250),
            "file async stop took {:?}",
            started.elapsed()
        );
    });
}

#[cfg(all(feature = "file-backend", feature = "async"))]
#[test]
fn file_capture_uses_joinable_thread_inside_tokio_runtime() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (dir, path) = write_temp_png("styx-file-async-stop");
        let device = crate::capture_api::make_file_device("file-async-stop", vec![path], 1, true);
        let handle = crate::capture_api::CaptureRequest::new(&device)
            .backend(crate::BackendKind::File)
            .start()
            .expect("file capture");

        assert!(
            matches!(&handle.worker, Some(WorkerHandle::Thread(_))),
            "file backend should use a joinable thread so sync stop waits for completion"
        );

        let started = Instant::now();
        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            started.elapsed() < Duration::from_millis(250),
            "file stop inside tokio runtime took {:?}",
            started.elapsed()
        );
    });
}

#[cfg(feature = "file-backend")]
fn write_temp_png(prefix: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("frame.png");
    let mut image = image::RgbImage::new(2, 2);
    for pixel in image.pixels_mut() {
        *pixel = image::Rgb([0x33, 0x66, 0x99]);
    }
    image.save(&path).expect("write png");
    (dir, path)
}
