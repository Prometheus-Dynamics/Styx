use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::capture_api::{CaptureRequest, StyxConfig, make_netcam_device};

fn unused_local_url() -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind local test port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    format!("http://127.0.0.1:{port}/mjpeg")
}

fn stalling_http_url() -> (String, mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind local test port");
    let port = listener.local_addr().expect("local addr").port();
    let (stop_tx, stop_rx) = mpsc::channel();
    let join = std::thread::spawn(move || {
        if let Ok((_stream, _addr)) = listener.accept() {
            let _ = stop_rx.recv_timeout(Duration::from_secs(2));
        }
    });
    (format!("http://127.0.0.1:{port}/mjpeg"), stop_tx, join)
}

#[test]
fn stop_interrupts_disconnected_netcam_backoff() {
    let config = StyxConfig::new()
        .netcam_timeouts(1)
        .netcam_http_timeouts(1, 100, 100)
        .netcam_backoff(5_000, 5_000);

    let device = make_netcam_device("disconnected-netcam", &unused_local_url(), 16, 16, 30);
    let handle = CaptureRequest::new(&device)
        .config(config)
        .start()
        .expect("start disconnected netcam worker");
    std::thread::sleep(Duration::from_millis(100));

    let start = Instant::now();
    handle.stop();
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(500),
        "stop took {elapsed:?}; worker likely waited through retry backoff"
    );
}

#[test]
fn disconnected_netcam_records_recent_worker_error() {
    let config = StyxConfig::new()
        .netcam_timeouts(1)
        .netcam_http_timeouts(1, 100, 100)
        .netcam_backoff(5_000, 5_000);

    let device = make_netcam_device("disconnected-netcam-error", &unused_local_url(), 16, 16, 30);
    let handle = CaptureRequest::new(&device)
        .config(config)
        .start()
        .expect("start disconnected netcam worker");
    let started = Instant::now();
    while handle.last_error().is_none() && started.elapsed() < Duration::from_secs(1) {
        std::thread::sleep(Duration::from_millis(10));
    }

    let error = handle
        .last_error()
        .expect("netcam worker should record request failures");
    assert!(error.to_string().contains("netcam request failed"));
    handle.stop();
}

#[test]
fn stop_is_bounded_while_netcam_http_read_is_stalled() {
    let (url, server_stop, server_join) = stalling_http_url();
    let config = StyxConfig::new()
        .netcam_http_timeouts(1, 100, 100)
        .netcam_backoff(5_000, 5_000);

    let device = make_netcam_device("stalled-netcam", &url, 16, 16, 30);
    let handle = CaptureRequest::new(&device)
        .config(config)
        .start()
        .expect("start stalled netcam worker");
    std::thread::sleep(Duration::from_millis(100));

    let start = Instant::now();
    handle.stop();
    let elapsed = start.elapsed();
    let _ = server_stop.send(());
    let _ = server_join.join();

    assert!(
        elapsed < Duration::from_millis(1_500),
        "stop took {elapsed:?}; worker likely waited past configured HTTP timeout"
    );
}

#[cfg(feature = "async")]
#[test]
fn async_stop_interrupts_stalled_netcam_request() {
    let (url, server_stop, server_join) = stalling_http_url();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime");

    runtime.block_on(async move {
        let config = StyxConfig::new()
            .netcam_http_timeouts(5, 1_000, 1_000)
            .netcam_stop_poll(10)
            .netcam_backoff(5_000, 5_000);
        let device = make_netcam_device("async-stalled-netcam", &url, 16, 16, 30);
        let handle = CaptureRequest::new(&device)
            .config(config)
            .start()
            .expect("start async stalled netcam worker");
        tokio::time::sleep(Duration::from_millis(100)).await;

        let start = Instant::now();
        handle.stop_async().await;
        let elapsed = start.elapsed();
        let _ = server_stop.send(());
        let _ = server_join.join();

        assert!(
            elapsed < Duration::from_millis(500),
            "async stop took {elapsed:?}; request future was not cooperatively cancelled"
        );
    });
}
