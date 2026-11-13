//! Runs each example with `--sim` (the simulated camera, no hardware) and checks that it exits 0
//! and prints the line that proves it ran.

use std::process::{Command, Output};

/// The built example `name`. `cargo test` builds the examples into `target/<profile>/examples`,
/// next to this test's `deps` directory.
fn example(name: &str) -> String {
    let test = std::env::current_exe().expect("the test binary's path");
    let profile = test
        .parent()
        .and_then(|deps| deps.parent())
        .expect("the target profile directory");
    profile.join("examples").join(name).display().to_string()
}

/// Runs `exe --sim [extra]` and returns its output. A failing exit is a test failure, with the
/// example's own output.
fn run_sim(exe: &str, name: &str, extra: &[&str]) -> Output {
    let out =
        std::env::temp_dir().join(format!("styx-examples-test-{name}-{}", std::process::id()));
    let output = Command::new(exe)
        .arg("--sim")
        .arg("--out")
        .arg(&out)
        .args(extra)
        .output()
        .unwrap_or_else(|e| panic!("cannot run {exe}: {e}"));
    let _ = std::fs::remove_dir_all(&out);
    assert!(
        output.status.success(),
        "{name} exited with {:?}:\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn assert_line(output: &Output, expected: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|line| line.contains(expected)),
        "no line contains {expected:?} in:\n{stdout}"
    );
}

#[test]
fn quickstart_reads_ten_frames() {
    let output = run_sim(&example("quickstart"), "quickstart", &[]);
    assert_line(&output, "achieved");
    let frames = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| l.starts_with("frame id="))
        .count();
    assert_eq!(frames, 10);
}

#[test]
fn capture_lands_controls_on_the_named_frame() {
    let output = run_sim(&example("capture"), "capture", &[]);
    assert_line(&output, "landed on frame 6, the frame named");
}

#[test]
fn socket_reader_reads_with_no_copy() {
    let output = run_sim(&example("socket_reader"), "socket_reader", &[]);
    assert_line(&output, "reader: delivered=10");
    assert_line(&output, "copies=0");
}

#[test]
fn daedalus_graph_or_how_to_enable_it() {
    let exe = example("daedalus_frames");
    if cfg!(feature = "daedalus") {
        let output = run_sim(&exe, "daedalus_frames", &[]);
        assert_line(&output, "reader: delivered=10");
    } else {
        let output = Command::new(&exe).output().expect("run daedalus_frames");
        assert!(output.status.success(), "exit {:?}", output.status.code());
        assert_line(&output, "needs the daedalus feature");
    }
}
