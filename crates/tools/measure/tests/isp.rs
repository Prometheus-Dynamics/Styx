//! `styx-measure read` on a camera's processed socket (isp0) with the simulated PiSP back end:
//! the after-ISP column of P3-2. Every frame is counted as the back end's, with its capture-to-
//! reader latency; the service's input pass is read with the bytes it read and wrote.
//!
//! Its own binary: the service's copy counters are process-wide.

#[path = "support/mod.rs"]
mod support;

use std::process::Command;
use std::time::Duration;

use styx_service::isp_socket;
use support::{
    CAMERA, daemon_with_back_end, list, start_service_with, test_dir, wait_publishing, wait_until,
};

#[test]
fn the_processed_socket_is_measured_by_maker_with_the_input_pass_read() {
    let dir = test_dir("isp");
    let lemnos = dir.path().join("lemnosd.sock");
    let _daemon = daemon_with_back_end(&lemnos);
    let service = start_service_with(dir.path(), &lemnos, "isp = \"pisp\"\n");
    wait_publishing(&service);
    wait_until("isp=pisp", Duration::from_secs(20), || {
        list(&service).iter().any(|l| l.contains(" isp=pisp "))
    });
    let socket = isp_socket(dir.path(), CAMERA, 0);
    let report = dir.path().join("isp0.json");
    let out = Command::new(env!("CARGO_BIN_EXE_styx-measure"))
        .args([
            "read",
            "--socket",
            &socket.display().to_string(),
            "--control",
            &service.control_socket().display().to_string(),
            "--report",
            &report.display().to_string(),
            "--frames",
            "60",
        ])
        .output()
        .expect("run styx-measure");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).expect("the report")).expect("json");
    let made_by = &v["made_by"];
    assert_eq!(made_by["pisp"]["frames"].as_u64(), Some(60), "{made_by}");
    assert_eq!(made_by["camera"]["frames"].as_u64(), Some(0), "{made_by}");
    assert_eq!(made_by["soft"]["frames"].as_u64(), Some(0), "{made_by}");
    assert_eq!(made_by["unknown"].as_u64(), Some(0), "{made_by}");
    assert!(
        made_by["pisp"]["latency_median_ns"].as_u64().is_some(),
        "{made_by}"
    );
    assert!(
        v["service_camera"]["end"].is_object(),
        "the camera's line was found by its isp0 socket: {v}"
    );
    let input = v["copies"]["service"]
        .as_array()
        .and_then(|sites| sites.iter().find(|s| s["site"] == "pisp-be-input"))
        .unwrap_or_else(|| panic!("no pisp-be-input copy site: {v}"));
    // 320 x 40 RAW10 packed is 16000 bytes read; 16-bit samples are 25600 written, per pass.
    let (copies, read, written) = (
        input["copies"].as_u64().expect("copies"),
        input["bytes_read"].as_u64().expect("bytes_read"),
        input["bytes"].as_u64().expect("bytes"),
    );
    assert!(copies > 0, "{input}");
    assert_eq!(
        (read / copies, written / copies),
        (16_000, 25_600),
        "{input}"
    );
    println!("proof: isp0 measured {made_by} input pass {input}");
}
