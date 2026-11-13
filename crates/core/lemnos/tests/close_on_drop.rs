//! A dropped `Frames` closes its stream on the daemon, so the same stream opens again.
//!
//! Before this, a dropped `Frames` left its stream open on `lemnosd`, and the next `open_frames`
//! of that stream waited on it. This runs the receiver's stream open, read and drop 20 times on the
//! simulated backend, then an explicit `close()`.

#[allow(dead_code)]
mod sim;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use lemnos_contract_camera_receiver::{CameraReceiver, Packing};
use lemnos_contract_camera_sensor::CameraSensor;
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_format::FourCc;
use styx_lemnos::{Client, Frames, find_chain, open_frames};

fn start(name: &str, text: &str) -> (Service, PathBuf) {
    let dir = std::env::temp_dir().join(format!("styx-lemnos-close-{}", std::process::id()));
    let dir = dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("sock");
    let config = Config {
        socket: socket.clone(),
        name: "styx-lemnos-test".to_string(),
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

#[test]
fn a_dropped_frame_stream_reopens_twenty_times_and_close_returns_the_answer() {
    let (service, socket) = start("reopen", &sim::board(Some("csi0")));
    let client = Client::connect(&socket, "styx-lemnos-test").expect("connect");

    let devices = client.list().expect("list");
    let chain = find_chain(&devices).expect("chain by tag and link");
    let mut sensor = client
        .open::<dyn CameraSensor>(&chain.sensor.name)
        .expect("sensor contract");
    let mut receiver = client
        .open::<dyn CameraReceiver>(&chain.receiver.name)
        .expect("receiver contract");
    let mode = sensor.mode(0).expect("mode 0");
    sensor.select_mode(0).expect("select_mode");
    let layout = receiver
        .configure(mode.link, Packing::Packed)
        .expect("configure receiver");
    receiver.start().expect("receiver start");
    sensor.start().expect("sensor start");

    for round in 0..20 {
        let started = Instant::now();
        let mut frames: Frames = open_frames(
            &client,
            chain.receiver.index,
            "frames",
            FourCc::RGGB10P,
            layout.width,
            layout.height,
        )
        .unwrap_or_else(|e| panic!("round {round}: open_frames: {e}"));
        let mut seen = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !seen && Instant::now() < deadline {
            if let Some(lease) = frames.next(Duration::from_millis(200)).expect("next") {
                frames.release(lease);
                seen = true;
            }
        }
        assert!(seen, "round {round}: no frame");
        // Dropped, not closed: the drop must give the stream back.
        drop(frames);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "round {round} took {:?}",
            started.elapsed()
        );
    }

    // The explicit close returns the daemon's answer, and the stream opens again after it.
    let frames: Frames = open_frames(
        &client,
        chain.receiver.index,
        "frames",
        FourCc::RGGB10P,
        layout.width,
        layout.height,
    )
    .expect("open after 20 drops");
    frames.close().expect("close");
    let again: Frames = open_frames(
        &client,
        chain.receiver.index,
        "frames",
        FourCc::RGGB10P,
        layout.width,
        layout.height,
    )
    .expect("open after close");
    drop(again);

    drop(sensor);
    drop(receiver);
    drop(client);
    service.stop();
}
