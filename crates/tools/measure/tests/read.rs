//! `styx-measure read` and `status` as child processes, against the service in this process on
//! the simulated camera (120 fps, 320 x 40 raw Bayer, RGGB10P).
//!
//! Gaps: the service's ring can skip frames for a reader (its buffers are reclaimed after each
//! publish). The run measures that and does not hide it; the test asserts that every gap is a
//! counted skip, and reports both.

#[path = "support/mod.rs"]
mod support;

use std::process::Command;

use support::{CAMERA, daemon, socket_of, start_service, test_dir, wait_publishing};

const FRAMES: u64 = 1000;

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
    let lemnos = dir.join("lemnos.sock");
    let _daemon = daemon(&lemnos);
    let service = start_service(&dir, &lemnos);
    wait_publishing(&service);
    let frame_socket = socket_of(&dir);
    let control = service.control_socket().display().to_string();
    let frame_socket_text = frame_socket.display().to_string();
    let frames = FRAMES.to_string();

    // Without the ISP.
    let plain = dir.join("plain.json");
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
    let with_isp = dir.join("isp.json");
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
    // Every missing id is one the ring skipped (see the note in the test's header): a gap with no
    // skip would be a frame the reader lost by itself.
    let gaps = json_u64(&v, &["frames", "gaps"]);
    let skipped = json_u64(&v, &["frames", "skipped_by_ring"]);
    assert!(gaps <= skipped, "{gaps} gaps but {skipped} skips: {text}");
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
