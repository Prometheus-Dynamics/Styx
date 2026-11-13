//! Native cameras on Lemnos's simulated backend, through the real socket.
//!
//! `lemnosd` runs in this process with the simulated sensor, receiver and ISP drivers (see
//! `sim/mod.rs`) on `lemnos-sim`. [`styx_native::discover`] lists the board's devices through
//! `lemnos-client` and resolves every sensor to its camera.

mod sim;

use std::path::PathBuf;
use std::time::Duration;

use lemnos_contract_camera_sensor::LinkCheck;
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_lemnos::{ChainError, Client, connect};
use styx_native::{Camera, Discovery, Refusal, discover};

fn isp_names(camera: &Camera) -> Vec<&str> {
    camera.isps().iter().map(|d| d.name.as_str()).collect()
}

fn socket_dir() -> PathBuf {
    // Under the target directory, on the build's disk, not in the system temp directory.
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("styx-native-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start(name: &str, text: &str) -> (Service, PathBuf) {
    let dir = socket_dir().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("sock");
    let config = Config {
        socket: socket.clone(),
        name: "styx-native-test".to_string(),
        board: parse(text).expect("board"),
        drivers: vec![
            entry::<sim::SimSensor>(),
            entry::<sim::SimReceiver>(),
            entry::<sim::SimIsp>(),
        ],
        backend: Box::new(SimBoard::new()),
        options: Options {
            call_deadline: Duration::from_millis(200),
            ..Options::default()
        },
    };
    (Service::start(config).expect("start lemnosd"), socket)
}

fn discover_on(name: &str, text: &str) -> Discovery {
    let (service, socket) = start(name, text);
    let client: Client = connect(&socket, "styx-native-test").expect("connect");
    let found = discover(&client).expect("discover");
    drop(client);
    service.stop();
    found
}

#[test]
fn one_sensor_on_the_simulated_board_is_one_camera() {
    let found = discover_on("one", &sim::board(Some("csi0")));
    assert_eq!(found.cameras.len(), 1, "{found:?}");
    assert!(found.refused.is_empty(), "{:?}", found.refused);
    let camera = &found.cameras[0];
    assert_eq!(camera.name(), "cam0-sensor");
    assert_eq!(camera.sensor().name, "cam0-sensor");
    assert_eq!(camera.receiver().name, "csi0");
    assert_eq!(isp_names(camera), ["isp0"]);
    println!(
        "one-sensor board: cameras={} refused={} camera={} receiver={} isp={}",
        found.cameras.len(),
        found.refused.len(),
        camera.name(),
        camera.receiver().name,
        isp_names(camera).first().copied().unwrap_or("-"),
    );
}

#[test]
fn two_sensors_on_the_simulated_board_are_two_cameras() {
    let found = discover_on("two", &sim::board_with_two_sensors());
    let names: Vec<&str> = found.cameras.iter().map(|c| c.name()).collect();
    assert_eq!(names, ["cam0-sensor", "cam1-sensor"], "{found:?}");
    assert!(found.refused.is_empty(), "{:?}", found.refused);
    for camera in &found.cameras {
        assert_eq!(camera.receiver().name, "csi0");
        assert_eq!(isp_names(camera), ["isp0"]);
    }
    println!(
        "two-sensor board: cameras={} refused={} names={names:?}",
        found.cameras.len(),
        found.refused.len(),
    );
}

#[test]
fn an_unlinked_sensor_is_refused_and_the_linked_one_still_found() {
    // cam0-sensor has no link; cam1-sensor links to the receiver.
    let text = format!(
        "{}[[devices]]\nid = \"cam1-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi0\"\n",
        sim::board(None)
    );
    let found = discover_on("unlinked", &text);
    let names: Vec<&str> = found.cameras.iter().map(|c| c.name()).collect();
    assert_eq!(names, ["cam1-sensor"], "{found:?}");
    assert_eq!(
        found.refused,
        [(
            "cam0-sensor".to_string(),
            Refusal::Chain(ChainError::NotLinked("cam0-sensor".into()))
        )]
    );
}

fn discover_board(name: &str, text: &str) -> Discovery {
    discover_on(name, text)
}

#[test]
fn a_receiver_fed_by_this_sensor_confirms_the_link() {
    let text = sim::board_with(Some("csi0"), Some("cam-a"), Some("cam-a"));
    let found = discover_board("confirmed", &text);
    assert!(found.refused.is_empty(), "{:?}", found.refused);
    assert_eq!(found.cameras.len(), 1);
    assert_eq!(found.cameras[0].link(), &LinkCheck::Confirmed);
}

#[test]
fn a_receiver_fed_by_another_sensor_refuses_the_camera_naming_both() {
    let text = sim::board_with(Some("csi0"), Some("cam-a"), Some("cam-b"));
    let found = discover_board("mismatch", &text);
    assert!(found.cameras.is_empty(), "{found:?}");
    let [(name, refusal)] = found.refused.as_slice() else {
        panic!("one refusal: {:?}", found.refused);
    };
    assert_eq!(name, "cam0-sensor");
    assert_eq!(
        refusal,
        &Refusal::Mismatch {
            sensor: "cam0-sensor".into(),
            identity: "cam-a".into(),
            receiver: "csi0".into(),
            source: "cam-b".into(),
        }
    );
    let text = refusal.to_string();
    assert!(text.contains("cam-a") && text.contains("cam-b"), "{text}");
}

#[test]
fn an_empty_source_is_nothing_wired_and_refuses_the_camera() {
    let text = sim::board_with(Some("csi0"), Some("cam-a"), Some(""));
    let found = discover_board("unwired", &text);
    assert!(found.cameras.is_empty(), "{found:?}");
    assert_eq!(
        found.refused,
        [(
            "cam0-sensor".to_string(),
            Refusal::Unwired {
                sensor: "cam0-sensor".into(),
                receiver: "csi0".into(),
            }
        )]
    );
}

#[test]
fn an_unknown_source_is_carried_as_unknown_and_never_a_mismatch() {
    // The receiver cannot say what is wired (NOT_SUPPORTED): the camera stays, with the reason.
    let found = discover_board("unknown", &sim::board(Some("csi0")));
    assert!(found.refused.is_empty(), "{:?}", found.refused);
    assert!(
        matches!(found.cameras[0].link(), LinkCheck::Unknown(why) if why.contains("cannot say"))
    );

    // The sensor has no identity (NOT_SUPPORTED) and the receiver names one: still unknown, never a
    // match and never a mismatch.
    let text = sim::board_with(Some("csi0"), None, Some("cam-b"));
    let found = discover_board("no-identity", &text);
    assert!(found.refused.is_empty(), "{:?}", found.refused);
    assert!(matches!(found.cameras[0].link(), LinkCheck::Unknown(_)));
}
