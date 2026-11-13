//! Native cameras on Lemnos's simulated backend, through the real socket.
//!
//! `lemnosd` runs in this process with the simulated sensor, receiver and ISP drivers (see
//! `sim/mod.rs`) on `lemnos-sim`. [`styx_native::discover`] lists the board's devices through
//! `lemnos-client` and resolves every sensor to its camera.

mod sim;

use std::path::PathBuf;
use std::time::Duration;

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_lemnos::{ChainError, Client, connect};
use styx_native::{Discovery, discover};

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
    assert_eq!(camera.isp().map(|d| d.name.as_str()), Some("isp0"));
    println!(
        "one-sensor board: cameras={} refused={} camera={} receiver={} isp={}",
        found.cameras.len(),
        found.refused.len(),
        camera.name(),
        camera.receiver().name,
        camera.isp().map_or("-", |d| d.name.as_str()),
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
        assert_eq!(camera.isp().map(|d| d.name.as_str()), Some("isp0"));
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
            ChainError::NotLinked("cam0-sensor".into())
        )]
    );
}
