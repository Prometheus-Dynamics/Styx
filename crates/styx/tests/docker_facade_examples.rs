mod support;

use support::docker_facade::{DockerFacade, output_text};

#[test]
#[ignore = "requires docker"]
fn docker_capture_virtual_reports_virtual_frames() {
    let facade = DockerFacade::start();
    let output = facade.run_output("/workspace/target/debug/examples/capture_virtual");
    assert!(
        output.status.success(),
        "capture_virtual failed\n{}",
        output_text(&output)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("virtual capture on Virtual"));
    assert!(stdout.contains("#12"));
    assert!(stdout.contains("capture samples="));
}

#[test]
#[ignore = "requires docker"]
fn docker_low_latency_preview_reports_pipeline_health() {
    let facade = DockerFacade::start();
    let output = facade.run_output("/workspace/target/debug/examples/low_latency_preview");
    assert!(
        output.status.success(),
        "low_latency_preview failed\n{}",
        output_text(&output)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("#060"));
    assert!(stdout.contains("preview fps="));
    assert!(stdout.contains("copies="));
}

#[test]
#[ignore = "requires docker"]
fn docker_async_pipeline_reports_async_samples() {
    let facade = DockerFacade::start();
    let output = facade.run_output("/workspace/target/debug/examples/async_pipeline");
    assert!(
        output.status.success(),
        "async_pipeline failed\n{}",
        output_text(&output)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("#25"));
    assert!(stdout.contains("async capture avg_ms="));
    assert!(stdout.contains("samples="));
}

#[test]
#[ignore = "requires docker"]
fn docker_reliable_recording_writes_frames() {
    let facade = DockerFacade::start();
    let output = facade.run_output(
        "rm -rf /tmp/styx-recordings && /workspace/target/debug/examples/reliable_recording /tmp/styx-recordings 6 && test -f /tmp/styx-recordings/frame_000000.png && echo frame_000000.png:ok",
    );
    assert!(
        output.status.success(),
        "reliable_recording failed\n{}",
        output_text(&output)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("recorded 6 frames to /tmp/styx-recordings"));
    assert!(stdout.contains("frame_000000.png:ok"));
}
