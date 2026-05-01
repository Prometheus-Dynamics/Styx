use std::net::TcpListener;
use std::sync::mpsc;
#[cfg(feature = "async")]
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
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

#[cfg(feature = "async")]
fn ending_mjpeg_url() -> (
    String,
    Arc<AtomicUsize>,
    mpsc::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    use std::io::Write;

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind local test port");
    listener
        .set_nonblocking(true)
        .expect("set listener nonblocking");
    let port = listener.local_addr().expect("local addr").port();
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_for_thread = Arc::clone(&requests);
    let (stop_tx, stop_rx) = mpsc::channel();
    let join = std::thread::spawn(move || {
        while stop_rx.try_recv().is_err() {
            match listener.accept() {
                Ok((mut stream, _addr)) => {
                    requests_for_thread.fetch_add(1, Ordering::Relaxed);
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nConnection: close\r\n\r\n",
                    );
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    (
        format!("http://127.0.0.1:{port}/mjpeg"),
        requests,
        stop_tx,
        join,
    )
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
    let error = error.to_string();
    assert!(error.contains("netcam"));
    assert!(error.contains("failed"));
    handle.stop();
}

#[test]
fn disconnected_netcam_records_retry_health() {
    let config = StyxConfig::new()
        .netcam_timeouts(1)
        .netcam_http_timeouts(1, 100, 100)
        .netcam_backoff(100, 100);

    let device = make_netcam_device(
        "disconnected-netcam-retries",
        &unused_local_url(),
        16,
        16,
        30,
    );
    let handle = CaptureRequest::new(&device)
        .config(config)
        .start()
        .expect("start disconnected netcam worker");
    let started = Instant::now();
    let mut retries = 0;
    while retries == 0 && started.elapsed() < Duration::from_secs(1) {
        retries = handle.health_report().capture_retries.netcam_retry_count;
        std::thread::sleep(Duration::from_millis(10));
    }

    let retry_stats = handle.health_report().capture_retries;
    assert!(retry_stats.netcam_retry_count > 0);
    assert_eq!(
        retry_stats.last_retry_reason.as_deref(),
        Some("netcam_backoff")
    );
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

#[cfg(feature = "async")]
#[test]
fn async_drop_of_thread_backed_netcam_does_not_join_blocking_worker() {
    let (url, server_stop, server_join) = stalling_http_url();
    let config = StyxConfig::new()
        .netcam_http_timeouts(5, 1_000, 5_000)
        .netcam_backoff(5_000, 5_000);
    let device = make_netcam_device("async-drop-thread-netcam", &url, 16, 16, 30);
    let handle = CaptureRequest::new(&device)
        .config(config)
        .start()
        .expect("start thread-backed stalled netcam worker");

    std::thread::sleep(Duration::from_millis(100));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime");

    runtime.block_on(async move {
        let start = Instant::now();
        drop(handle);
        let elapsed = start.elapsed();
        let _ = server_stop.send(());
        let _ = server_join.join();

        assert!(
            elapsed < Duration::from_millis(500),
            "async drop took {elapsed:?}; drop joined a blocking thread worker"
        );
    });
}

#[cfg(feature = "async")]
#[test]
fn async_ended_mjpeg_stream_backs_off_instead_of_reconnecting_tightly() {
    let (url, requests, server_stop, server_join) = ending_mjpeg_url();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime");

    runtime.block_on(async move {
        let config = StyxConfig::new()
            .netcam_http_timeouts(1, 100, 100)
            .netcam_stop_poll(10)
            .netcam_backoff(5_000, 5_000);
        let device = make_netcam_device("async-ending-mjpeg-netcam", &url, 16, 16, 30);
        let handle = CaptureRequest::new(&device)
            .config(config)
            .start()
            .expect("start async ending netcam worker");

        tokio::time::sleep(Duration::from_millis(250)).await;
        let request_count = requests.load(Ordering::Relaxed);
        handle.stop_async().await;
        let _ = server_stop.send(());
        let _ = server_join.join();

        assert!(
            request_count <= 2,
            "async netcam reconnected {request_count} times before backoff"
        );
    });
}
