//! Connecting with no service there: at once without a bound, at the bound with one.

use std::time::{Duration, Instant};

use styx_lemnos::{Error, connect, connect_within};

/// A path no service listens on, inside a directory this test owns.
fn absent() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("lemnos.sock");
    (dir, path)
}

#[test]
fn no_service_fails_at_once() {
    let (_dir, path) = absent();
    let start = Instant::now();
    let result = connect(&path, "styx-connect-test");
    let took = start.elapsed();
    assert!(result.is_err(), "connected to nothing");
    // The old client slept a retry step (1 s) before saying no.
    assert!(took < Duration::from_millis(100), "took {took:?}");
}

#[test]
fn a_bound_waits_for_the_service_then_times_out() {
    let (_dir, path) = absent();
    let bound = Duration::from_millis(300);
    let start = Instant::now();
    let result = connect_within(&path, "styx-connect-test", bound);
    let took = start.elapsed();
    assert!(
        matches!(result, Err(Error::Client(lemnos_client::Error::TimedOut))),
        "{result:?}"
    );
    assert!(took >= bound, "gave up early: {took:?}");
    assert!(took < bound + Duration::from_millis(700), "took {took:?}");
}
