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
fn docker_capture_and_decode_reports_pipeline_metrics() {
    let facade = DockerFacade::start();
    let output = facade.run_output("/workspace/target/debug/examples/capture_and_decode");
    assert!(
        output.status.success(),
        "capture_and_decode failed\n{}",
        output_text(&output)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("#30"));
    assert!(stdout.contains("capture avg_ms="));
    assert!(stdout.contains("decode avg_ms="));
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
fn docker_record_and_replay_writes_and_replays_frames() {
    let facade = DockerFacade::start();
    let output = facade.run_output(
        "rm -rf /tmp/styx-recordings && /workspace/target/debug/examples/record_and_replay /tmp/styx-recordings 6 && test -f /tmp/styx-recordings/frame-0000.png && echo frame-0000.png:ok",
    );
    assert!(
        output.status.success(),
        "record_and_replay failed\n{}",
        output_text(&output)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("recorded 6 frames to /tmp/styx-recordings"));
    assert!(stdout.contains("replay #5"));
    assert!(stdout.contains("frame-0000.png:ok"));
}
