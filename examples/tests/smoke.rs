use std::process::{Command, Output};

fn assert_success(output: Output, name: &str) -> String {
    assert!(
        output.status.success(),
        "{name} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("example stdout should be utf-8")
}

#[test]
fn quickstart_capture_virtual_reports_frames() {
    let stdout = assert_success(
        Command::new(env!("CARGO_BIN_EXE_quickstart_capture_virtual"))
            .output()
            .expect("quickstart_capture_virtual should run"),
        "quickstart_capture_virtual",
    );

    assert!(stdout.contains("virtual capture on Virtual"));
    assert!(stdout.contains("#12"));
    assert!(stdout.contains("capture samples=12"));
}

#[test]
fn quickstart_pipeline_health_reports_metrics() {
    let stdout = assert_success(
        Command::new(env!("CARGO_BIN_EXE_quickstart_pipeline_health"))
            .output()
            .expect("quickstart_pipeline_health should run"),
        "quickstart_pipeline_health",
    );

    assert!(stdout.contains("processed_frames="));
    assert!(stdout.contains("copies="));
    assert!(stdout.contains("graph_copied_bytes="));
}

#[test]
fn latest_frame_fanout_reports_branch_counts() {
    let stdout = assert_success(
        Command::new(env!("CARGO_BIN_EXE_latest_frame_fanout"))
            .output()
            .expect("latest_frame_fanout should run"),
        "latest_frame_fanout",
    );

    assert!(stdout.contains("fanout pushed="));
    assert!(stdout.contains("preview_seen="));
    assert!(stdout.contains("analysis_seen="));
}
