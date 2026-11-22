//! `styx-measure read` and `status` as child processes, against the service in this process on
//! the simulated camera (120 fps, 320 x 40 raw Bayer, RGGB10P).
//!
//! Gaps: the service's ring keeps the newest frames for readers (its keep window), so a reader that
//! keeps up sees every frame: the test asserts no gap and no skip. A reader later than the window
//! would be counted as skips; this one is not.

use crate::support;

use std::process::Command;

use support::{CAMERA, daemon, socket_of, start_service, test_dir, wait_publishing};

/// Frames per read: the measured rate is the assertion, so the camera runs at its real 120 fps;
/// 200 frames give the rate within the 1 % asserted below.
const FRAMES: u64 = 200;

fn run_measure(args: &[&str]) -> (String, String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_styx-measure"))
        .args(args)
        .output()
        .expect("run styx-measure");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

fn json_u64(v: &serde_json::Value, path: &[&str]) -> u64 {
    let mut cur = v;
    for key in path {
        cur = cur.get(*key).unwrap_or_else(|| panic!("missing {path:?}"));
    }
    cur.as_u64()
        .unwrap_or_else(|| panic!("{path:?} is not a number: {cur}"))
}

fn json_f64(v: &serde_json::Value, path: &[&str]) -> f64 {
    let mut cur = v;
    for key in path {
        cur = cur.get(*key).unwrap_or_else(|| panic!("missing {path:?}"));
    }
    cur.as_f64()
        .unwrap_or_else(|| panic!("{path:?} is not a number: {cur}"))
}

#[test]
fn read_measures_the_simulated_camera_with_and_without_the_soft_isp() {
    let dir = test_dir("read");
    let lemnos = dir.path().join("lemnos.sock");
    let _daemon = daemon(&lemnos);
    let service = start_service(dir.path(), &lemnos);
    wait_publishing(&service);
    let frame_socket = socket_of(dir.path());
    let control = service.control_socket().display().to_string();
    let frame_socket_text = frame_socket.display().to_string();
    let frames = FRAMES.to_string();

    // Without the ISP.
    let plain = dir.path().join("plain.json");
    let (stdout, stderr, ok) = run_measure(&[
        "read",
        "--socket",
        &frame_socket_text,
        "--control",
        &control,
        "--frames",
        &frames,
        "--asked-fps",
        "120.00",
        "--service-pid",
        &std::process::id().to_string(),
        "--report",
        &plain.display().to_string(),
    ]);
    assert!(ok, "read failed: {stderr}\n{stdout}");
    println!("{stdout}");
    check_report(&plain, false);

    // With the soft ISP on each frame, into NV12 at the raw size.
    let with_isp = dir.path().join("isp.json");
    let (stdout, stderr, ok) = run_measure(&[
        "read",
        "--socket",
        &frame_socket_text,
        "--control",
        &control,
        "--frames",
        &frames,
        "--soft-isp",
        "NV12",
        "--report",
        &with_isp.display().to_string(),
    ]);
    assert!(ok, "read --soft-isp failed: {stderr}\n{stdout}");
    println!("{stdout}");
    check_report(&with_isp, true);

    // The status subcommand prints the list answer: the camera's line and the `ok`.
    let (stdout, stderr, ok) = run_measure(&["status", "--control", &control]);
    assert!(ok, "status failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.last(), Some(&"ok"), "{stdout}");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with(&format!("camera {CAMERA} "))),
        "{stdout}"
    );
    println!("{stdout}");

    // Failures are reported with exit codes, not panics.
    let (_, stderr, ok) =
        run_measure(&["read", "--socket", "/nonexistent/socket", "--frames", "5"]);
    assert!(!ok && stderr.contains("connect"), "{stderr}");
    let (_, stderr, ok) = run_measure(&["read", "--socket", &frame_socket_text]);
    assert!(!ok && stderr.contains("--frames"), "{stderr}");

    service.shutdown();
}

fn check_report(path: &std::path::Path, isp: bool) {
    let text = std::fs::read_to_string(path).expect("report written");
    let v: serde_json::Value = serde_json::from_str(&text).expect("the report is JSON");

    // Every field the board-run record reads is there.
    for key in [
        "tool",
        "socket",
        "control",
        "stop",
        "wall_s",
        "asked",
        "frames",
        "rate",
        "latency_ns",
        "copies",
        "cpu",
        "memory",
        "isp",
        "service_camera",
    ] {
        assert!(v.get(key).is_some(), "report has no {key}: {text}");
    }

    assert_eq!(json_u64(&v, &["frames", "delivered"]), FRAMES, "{text}");
    // The claim: a reader that keeps up gets every frame, no gap and no skip, with or without the
    // ISP (checked below against the ring's window). Where the reader was late, the gaps must
    // still be counted skips, and that is what is asserted instead.
    let gaps = json_u64(&v, &["frames", "gaps"]);
    let skipped = json_u64(&v, &["frames", "skipped_by_ring"]);
    assert_eq!(json_u64(&v, &["frames", "duplicates"]), 0, "{text}");
    assert_eq!(json_u64(&v, &["frames", "out_of_order"]), 0, "{text}");
    assert_eq!(json_u64(&v, &["frames", "metadata_refused"]), 0, "{text}");
    assert_eq!(json_u64(&v, &["copies", "this_process_total"]), 0, "{text}");

    // The simulated sensor runs 320 x 40 at 120 fps: one frame each 8 333 333 ns.
    let fps = json_f64(&v, &["rate", "measured_fps"]);
    assert!(
        (fps - 120.0).abs() / 120.0 < 0.01,
        "measured {fps} fps: {text}"
    );
    assert!(json_f64(&v, &["rate", "mean_interval_ns"]) > 0.0);

    for key in ["min", "median", "p99", "max", "samples"] {
        assert!(v["latency_ns"].get(key).is_some(), "latency {key}: {text}");
    }
    assert!(json_u64(&v, &["latency_ns", "median"]) > 0, "{text}");
    assert!(json_u64(&v, &["latency_ns", "max"]) > 0, "{text}");

    assert!(
        v["memory"]["reader_start"]["rss_bytes"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
    assert!(json_f64(&v, &["cpu", "reader_ns_per_frame"]) > 0.0);
    assert!(v["service_camera"]["start"].is_object(), "{text}");
    assert_eq!(v["service_camera"]["end"]["state"], "streaming", "{text}");

    // The rates: requested (the config's 120 fps, or the operator's override), achieved by the
    // backend and in the metadata, and measured by the service, all within 1 % of 120 fps.
    let near_120 = |v: f64| (v - 120.0).abs() / 120.0 < 0.01;
    assert_eq!(json_f64(&v, &["rate", "requested_fps"]), 120.0, "{text}");
    for key in [
        "achieved_backend_fps",
        "achieved_metadata_fps",
        "measured_service_fps",
    ] {
        let rate = json_f64(&v, &["rate", key]);
        assert!(near_120(rate), "{key} {rate}: {text}");
    }
    // The backend runs at 119.967 fps (the frame length rounds to whole lines), so the reason says so.
    let reason = v["rate"]["rate_reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("whole lines") && reason.contains("119967"),
        "{text}"
    );
    // The service's copies, from its status: none on the frame path.
    assert_eq!(json_u64(&v, &["copies", "service_total"]), 0, "{text}");
    let period_ns = 1e9 / 120.0;
    // The camera's stream declares a hold of 4 of its 8 buffers; the frame in hand is one of
    // them, so the ring keeps 3 and the reserve is 5.
    let keep = json_u64(&v, &["service_camera", "end", "keep"]) as f64;
    assert_eq!(
        json_u64(&v, &["service_camera", "end", "hardware_reserve"]),
        5,
        "reserve is buffers 8 - hold 4 + the frame in hand: {text}"
    );
    assert_eq!(
        keep, 3.0,
        "keep is min(history 8, buffers 8 - reserve 5): {text}"
    );
    // "Keeps up" is checked, not assumed, and not from the latency of the frames the reader got:
    // a reader held up past frames the ring skipped gets a younger frame next, and its latency
    // looks fine. What decides a skip is how old the frame the reader wanted was when it came
    // back for it. While the reader has a frame in hand (the soft ISP's time included), that
    // frame is one of the `keep` and the ring keeps the newest `keep - 1` beside it; the next
    // frame is cut once `keep - 1` more are published. Published is no earlier than captured, so
    // the next frame is safe while it is younger than (keep - 1) periods when asked for: the time
    // since the previous capture is below `keep` periods.
    let bound_ns = keep * period_ns;
    let since_previous = json_u64(&v, &["latency_ns", "since_previous_capture_max"]) as f64;
    if since_previous >= bound_ns {
        println!(
            "previous capture to next frame max {since_previous} ns is not below keep frame \
             periods ({bound_ns} ns): the reader (or a process before it) was held up; zero gaps \
             is not asserted for this run ({gaps} gaps, {skipped} skips)"
        );
        assert!(gaps <= skipped, "{gaps} gaps but {skipped} skips: {text}");
    } else {
        assert_eq!(gaps, 0, "a reader that keeps up has no gaps: {text}");
        assert_eq!(skipped, 0, "the ring skipped no frame: {text}");
    }

    if isp {
        assert_eq!(v["isp"]["output"], "NV12", "{text}");
        assert_eq!(json_u64(&v, &["isp", "frames"]), FRAMES, "{text}");
        assert_eq!(json_u64(&v, &["isp", "errors"]), 0, "{text}");
        assert_eq!(json_u64(&v, &["isp", "pool", "misses"]), 0, "{text}");
        assert!(json_u64(&v, &["isp", "time_ns", "median"]) > 0, "{text}");
        assert!(json_u64(&v, &["isp", "time_ns", "p99"]) > 0, "{text}");
    } else {
        assert!(v["isp"].is_null(), "{text}");
    }
}
