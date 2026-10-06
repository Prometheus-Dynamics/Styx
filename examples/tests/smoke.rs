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
    assert!(stdout.contains("bytes_moved="));
}

#[test]
fn quickstart_runtime_memory_report_prints_process_and_pipeline_reports() {
    let stdout = assert_success(
        Command::new(env!("CARGO_BIN_EXE_quickstart_runtime_memory_report"))
            .output()
            .expect("quickstart_runtime_memory_report should run"),
        "quickstart_runtime_memory_report",
    );

    assert!(stdout.contains("process-only report before capture:"));
    assert!(stdout.contains("pipeline-attached report after"));
    assert!(stdout.contains("Process:"));
    assert!(stdout.contains("Styx tracked:"));
    assert!(stdout.contains("Unexplained PSS:"));
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

#[cfg(feature = "daedalus")]
#[test]
fn daedalus_frames_runs_camera_frames_through_a_graph() {
    let stdout = assert_success(
        Command::new(env!("CARGO_BIN_EXE_daedalus_frames"))
            .output()
            .expect("daedalus_frames should run"),
        "daedalus_frames",
    );

    // The planner put the metadata adapter on the frame -> descriptor edge.
    assert!(stdout.contains("styx.frame_descriptor"), "{stdout}");
    // ... and Styx's `daedalus:frame` provider before the `FrameView` node, which reads the
    // virtual camera's RGB24 (DRM BGR888) frames in place.
    assert!(
        stdout.contains("daedalus.foreign:styx:framelease->daedalus:frame"),
        "{stdout}"
    );
    assert!(stdout.contains("view=\"BG24 320x240"), "{stdout}");
    assert!(stdout.contains("mapped at"), "{stdout}");
    // Inspection shows frames as their descriptor, not an opaque summary.
    assert!(stdout.contains("\"residency\""), "{stdout}");
    assert!(stdout.contains("frames=8"), "{stdout}");
}
