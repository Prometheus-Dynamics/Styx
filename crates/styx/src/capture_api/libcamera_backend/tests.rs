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
