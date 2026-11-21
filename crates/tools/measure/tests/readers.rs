//! `styx-measure read --keep N` and `--stall-after N` against the service in this process on the
//! simulated camera: 8 buffers, a declared hold of 4, so the outlet's reserve is 5 and all readers
//! together may keep 2 frames across a publish (crates/out/socket/tests/reader_sets.rs, B = 8).
//! These are the reader shapes of the board sheet's hold runs; here they run on the sim first.

use crate::support;

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use styx_measure::cli::{Command as Cli, parse};
use support::{daemon, socket_of, start_service, test_dir, wait_publishing};

fn measure(socket: &Path, control: &Path, report: &Path, extra: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_styx-measure"))
        .args([
            "read",
            "--socket",
            &socket.display().to_string(),
            "--control",
            &control.display().to_string(),
            "--report",
            &report.display().to_string(),
        ])
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run styx-measure")
}

fn finish(child: Child) -> (bool, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = child.wait_with_output().expect("styx-measure ran");
    (
        status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        ),
    )
}

fn json(report: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(report).expect("the report")).expect("json")
}

fn cuts(v: &serde_json::Value) -> u64 {
    v["service_camera"]["end"]["reserve_cuts"]
        .as_u64()
        .unwrap_or_else(|| panic!("no reserve_cuts in {v}"))
}

/// One service; the cases run one after another against it, so the cut count is a running total.
#[test]
fn keep_and_stall_readers_against_the_share_of_two() {
    let dir = test_dir("readers");
    let lemnos = dir.path().join("lemnosd.sock");
    let _daemon = daemon(&lemnos);
    let service = start_service(dir.path(), &lemnos);
    wait_publishing(&service);
    let socket = socket_of(dir.path());
    let control = service.control_socket().to_path_buf();
    let report = |name: &str| dir.path().join(format!("{name}.json"));

    // A reader keeping its newest 2: the whole share, never cut.
    let (ok, text) = finish(measure(
        &socket,
        &control,
        &report("keep2"),
        &["--keep", "2", "--frames", "240"],
    ));
    assert!(ok, "--keep 2 alone: {text}");
    let v = json(&report("keep2"));
    assert_eq!(v["stop"], "frames", "{text}");
    assert_eq!(cuts(&v), 0, "{text}");

    // A stalled reader holding 2 of its own: the whole share, never cut; it ends on the time.
    let (ok, text) = finish(measure(
        &socket,
        &control,
        &report("stall2"),
        &["--stall-after", "2", "--seconds", "1.5"],
    ));
    assert!(ok, "--stall-after 2 alone: {text}");
    let v = json(&report("stall2"));
    assert_eq!(v["stop"], "time", "{text}");
    assert_eq!(cuts(&v), 0, "{text}");

    // A stalled reader holding 3: one past the share, cut off once with a consumer fault.
    let (ok, text) = finish(measure(
        &socket,
        &control,
        &report("stall3"),
        &["--stall-after", "3", "--seconds", "3"],
    ));
    assert!(!ok, "--stall-after 3 is cut: {text}");
    assert!(text.contains("frames held at the cut: 3"), "{text}");

    // The pair: a reader keeping 2 beside a stalled reader holding 1 of its own (3 distinct, one
    // past the share). The stalled reader holds the oldest frame and is the one cut; the reader
    // keeping 2 runs on.
    let keeper = measure(
        &socket,
        &control,
        &report("pair-keep"),
        &["--keep", "2", "--seconds", "2.5"],
    );
    let stalled = measure(
        &socket,
        &control,
        &report("pair-stall"),
        &["--stall-after", "1", "--seconds", "2.5"],
    );
    let (stalled_ok, stalled_text) = finish(stalled);
    let (keeper_ok, keeper_text) = finish(keeper);
    assert!(!stalled_ok, "the stalled reader is cut: {stalled_text}");
    assert!(
        stalled_text.contains("frames held at the cut: 1"),
        "{stalled_text}"
    );
    assert!(keeper_ok, "the reader keeping 2 runs on: {keeper_text}");
    let v = json(&report("pair-keep"));
    assert_eq!(v["stop"], "time", "{keeper_text}");
    assert_eq!(
        cuts(&v),
        2,
        "the stall-after 3 cut and this one: {keeper_text}"
    );
}

#[test]
fn the_reader_shapes_are_one_each_and_a_stall_needs_a_time() {
    let args = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
    for both in [
        "read --socket s --frames 1 --hold 1 --keep 1",
        "read --socket s --seconds 1 --keep 1 --stall-after 1",
    ] {
        let err = parse(&args(both)).expect_err(both);
        assert!(err.contains("one each"), "{both}: {err}");
    }
    let err = parse(&args("read --socket s --frames 5 --stall-after 1")).expect_err("no time");
    assert!(err.contains("needs --seconds"), "{err}");
    assert!(parse(&args("read --socket s --frames 1 --stall-after 0")).is_err());
    match parse(&args("read --socket s --seconds 1 --stall-after 2")) {
        Ok(Cli::Read(r)) => assert_eq!((r.stall_after, r.keep, r.hold), (Some(2), 0, 0)),
        other => panic!("{other:?}"),
    }
    match parse(&args("read --socket s --frames 9 --keep 3")) {
        Ok(Cli::Read(r)) => assert_eq!((r.keep, r.stall_after), (3, None)),
        other => panic!("{other:?}"),
    }
}
