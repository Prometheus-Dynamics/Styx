//! The client against a scripted control socket: one connection per request, the retry backoff,
//! the timeout, the wait for a change of controls, and the answer limits.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use styx_control_client::{ControlClient, ErrorKind, MAX_ANSWER, Options};
use styx_controls::{Control, ControlValue};

/// Serves `path` as the service does (one connection at a time, many requests per connection),
/// answering each request with `answer(request, n)`, n counting requests from 0. Returns the
/// count of connections taken.
fn serve(
    path: &Path,
    answer: impl Fn(&str, u64) -> Vec<String> + Send + 'static,
) -> Arc<AtomicU64> {
    let listener = UnixListener::bind(path).expect("bind");
    let connections = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&connections);
    thread::spawn(move || {
        let mut requests = 0u64;
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            counted.fetch_add(1, Ordering::SeqCst);
            let mut writer = stream.try_clone().expect("clone");
            let mut lines = BufReader::new(stream);
            let mut line = String::new();
            while lines.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                for out in answer(line.trim_end(), requests) {
                    if writeln!(writer, "{out}").is_err() {
                        break;
                    }
                }
                requests += 1;
                line.clear();
            }
        }
    });
    connections
}

fn camera_line(seq: u64) -> String {
    format!("camera cam0 state=streaming owner=native controls_seq={seq} last_error=-")
}

fn quick() -> Options {
    Options {
        timeout: Duration::from_millis(500),
        retry_first: Duration::from_millis(100),
        retry_cap: Duration::from_millis(400),
        poll: Duration::from_millis(20),
    }
}

#[test]
fn each_request_has_its_own_connection_and_its_answer() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("control.sock");
    let connections = serve(&path, |request, _| match request {
        "list" => vec![camera_line(3), "ok".into()],
        "frame cam0" => vec![
            "frame id=12 timestamp_ns=34 exposure_us=5000 gain_milli=2000".into(),
            "ok".into(),
        ],
        "set cam0 exposure 9000 40" => vec!["ok".into()],
        "set cam0 gain 99999 40" => {
            vec!["err OutOfRange { min: 1000, max: 15938, value: 99999 }".into()]
        }
        "set cam0 ae off 0" => vec!["ok".into()],
        other => vec![format!("err no camera {other}")],
    });
    let mut client = ControlClient::with_options(&path, quick());
    let listing = client.list().expect("list");
    assert_eq!(listing.cameras[0].controls_seq, Some(3));
    let frame = client.frame("cam0").expect("frame");
    assert_eq!(
        (frame.id, frame.timestamp_ns, frame.exposure_us),
        (12, 34, Some(5000))
    );
    client
        .set("cam0", Control::Exposure, ControlValue::Manual(9000), 40)
        .expect("set");
    let refused = client
        .set("cam0", Control::Gain, ControlValue::Manual(99_999), 40)
        .expect_err("out of range");
    assert_eq!(refused.kind, ErrorKind::Range);
    assert!(refused.message.contains("max: 15938"), "{refused}");
    client
        .set_named("cam0", "ae", "off", 0)
        .expect("a loop control by name");
    assert_eq!(
        client
            .camera("cam9")
            .map_err(|e| e.kind)
            .expect_err("absent"),
        ErrorKind::NotFound
    );
    assert_eq!(
        client
            .set_named("cam0", "ae", "off\nstop cam0", 0)
            .map_err(|e| e.kind),
        Err(ErrorKind::Invalid),
        "a value cannot add a request"
    );
    // Six requests went out (the invalid one was never sent): six connections.
    assert_eq!(connections.load(Ordering::SeqCst), 6);
}

#[test]
fn an_absent_service_is_retried_on_a_doubling_backoff_and_then_reached() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("control.sock");
    let mut client = ControlClient::with_options(&path, quick());

    let first = client.list().expect_err("no service yet");
    assert_eq!(first.kind, ErrorKind::Unreachable);
    assert!(
        client
            .retry_in()
            .is_some_and(|w| w <= Duration::from_millis(100))
    );

    // Before the wait is over a call fails at once, without touching the socket.
    let started = Instant::now();
    let early = client.list().expect_err("still waiting");
    assert_eq!(early.kind, ErrorKind::Unreachable);
    assert!(early.message.contains("tried again in"), "{early}");
    assert!(started.elapsed() < Duration::from_millis(20));

    // The second failed connect waits twice as long.
    thread::sleep(Duration::from_millis(110));
    assert_eq!(
        client.list().map_err(|e| e.kind).expect_err("still absent"),
        ErrorKind::Unreachable
    );
    let second = client.retry_in().expect("a wait");
    assert!(
        second > Duration::from_millis(100) && second <= Duration::from_millis(200),
        "{second:?}"
    );

    serve(&path, |_, _| vec![camera_line(1), "ok".into()]);
    thread::sleep(second + Duration::from_millis(10));
    assert_eq!(client.list().expect("reached").cameras.len(), 1);
    assert_eq!(client.retry_in(), None, "a connect resets the backoff");
}

#[test]
fn a_service_that_does_not_answer_times_out() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("control.sock");
    serve(&path, |_, _| {
        thread::sleep(Duration::from_secs(3));
        vec![]
    });
    let mut client = ControlClient::with_options(&path, quick());
    let started = Instant::now();
    let e = client.list().expect_err("no answer");
    assert_eq!(e.kind, ErrorKind::Timeout);
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(450) && took < Duration::from_millis(1500),
        "{took:?}"
    );
}

#[test]
fn a_wait_returns_the_new_count_once_it_moves_and_none_without_a_change() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("control.sock");
    // The count moves from 3 to 4 at the fifth request.
    serve(&path, |_, n| {
        vec![camera_line(if n < 4 { 3 } else { 4 }), "ok".into()]
    });
    let mut client = ControlClient::with_options(&path, quick());
    let started = Instant::now();
    assert_eq!(
        client.wait_controls_change("cam0", 3, Duration::from_secs(2)),
        Ok(Some(4))
    );
    // Four polls at 20 ms before the fifth.
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(75) && took < Duration::from_secs(1),
        "{took:?}"
    );
    let started = Instant::now();
    assert_eq!(
        client.wait_controls_change("cam0", 4, Duration::from_millis(150)),
        Ok(None)
    );
    assert!(started.elapsed() >= Duration::from_millis(150));
    assert_eq!(
        client
            .wait_controls_change("cam9", 4, Duration::from_millis(150))
            .map_err(|e| e.kind),
        Err(ErrorKind::NotFound),
        "an absent camera is an answer, not a wait"
    );
}

#[test]
fn a_wait_rides_out_a_service_that_comes_back() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("control.sock");
    let mut client = ControlClient::with_options(&path, quick());
    let late = path.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        serve(&late, |_, _| vec![camera_line(8), "ok".into()]);
    });
    assert_eq!(
        client.wait_controls_change("cam0", 7, Duration::from_secs(3)),
        Ok(Some(8))
    );
    let dir2 = tempfile::tempdir().expect("dir");
    let mut never = ControlClient::with_options(dir2.path().join("none.sock"), quick());
    assert_eq!(
        never
            .wait_controls_change("cam0", 7, Duration::from_millis(200))
            .map_err(|e| e.kind),
        Err(ErrorKind::Unreachable),
        "with no answer at all the last error is returned"
    );
}

#[test]
fn an_answer_past_the_limits_is_a_protocol_error() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("control.sock");
    serve(&path, |request, _| match request {
        "list" => (0..=MAX_ANSWER)
            .map(|_| "copy copies=0".to_string())
            .collect(),
        _ => vec!["x".repeat(70 * 1024)],
    });
    let mut client = ControlClient::with_options(&path, quick());
    assert_eq!(
        client.list().map_err(|e| e.kind).expect_err("too many"),
        ErrorKind::Protocol
    );
    assert_eq!(
        client
            .controls("cam0")
            .map_err(|e| e.kind)
            .expect_err("too long"),
        ErrorKind::Protocol
    );
}
