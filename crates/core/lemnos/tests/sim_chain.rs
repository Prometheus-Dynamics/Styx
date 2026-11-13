//! The camera chain on Lemnos's simulated backend, through the real socket.
//!
//! `lemnosd` runs in this process with the simulated sensor, receiver and ISP drivers (see
//! `sim/mod.rs`) on `lemnos-sim`. This crate finds the chain by tag and link through
//! `lemnos-client`, starts it with contract calls in the order the Lemnos contracts set, and
//! reads frames from the receiver's stream as `FrameLease`s.

mod sim;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use lemnos_contract_camera_isp::{CameraIsp, IspInput};
use lemnos_contract_camera_receiver::{CameraReceiver, Packing};
use lemnos_contract_camera_sensor::CameraSensor;
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_format::FourCc;
use styx_frame::copy_counters;
use styx_lemnos::{ChainError, Client, Frames, find_chain, open_frames};

fn socket_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-lemnos-sim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start(name: &str, text: &str) -> (Service, PathBuf) {
    let dir = socket_dir().join(name);
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
fn two_sensors_on_the_simulated_board_are_refused_by_tag() {
    let (service, socket) = start("two", &sim::board_with_two_sensors());
    let client = Client::connect(&socket, "styx-lemnos-test").expect("connect");
    let devices = client.list().expect("list");
    assert_eq!(find_chain(&devices), Err(ChainError::SensorCount(2)));
    drop(client);
    service.stop();
}

#[test]
fn chain_found_by_tag_and_link_and_frames_arrive_with_no_copies() {
    let (service, socket) = start("chain", &sim::board(Some("csi0")));
    let client = Client::connect(&socket, "styx-lemnos-test").expect("connect");

    let devices = client.list().expect("list");
    let chain = find_chain(&devices).expect("chain by tag and link");
    assert_eq!(chain.sensor.name, "cam0-sensor");
    assert_eq!(chain.receiver.name, "csi0");
    assert_eq!(
        chain
            .isps
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        ["isp0"]
    );

    // Sensor and receiver contract calls, in the order the Lemnos contracts set.
    let mut sensor = client
        .open::<dyn CameraSensor>(&chain.sensor.name)
        .expect("sensor contract");
    let mut receiver = client
        .open::<dyn CameraReceiver>(&chain.receiver.name)
        .expect("receiver contract");
    let mut isp = client
        .open::<dyn CameraIsp>(&chain.isps[0].name)
        .expect("isp contract");
    let mode = sensor.mode(0).expect("mode 0");
    sensor.select_mode(0).expect("select_mode");
    let layout = receiver
        .configure(mode.link, Packing::Packed)
        .expect("configure receiver");
    isp.set_input(IspInput {
        width: layout.width,
        height: layout.height,
        bit_depth: layout.bit_depth,
        bayer: mode.bayer,
    })
    .expect("set_input");

    // The receiver's frame stream, opened through the client and mapped without copies.
    let mut frames: Frames = open_frames(
        &client,
        chain.receiver.index,
        "frames",
        FourCc::RGGB10P,
        layout.width,
        layout.height,
    )
    .expect("open frame stream");
    assert_eq!(frames.buffers(), 8);

    receiver.start().expect("receiver start");
    sensor.start().expect("sensor start");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = 0usize;
    let mut last_id = None;
    while seen < 12 && Instant::now() < deadline {
        if let Some(lease) = frames.next(Duration::from_millis(200)).expect("next") {
            assert!(lease.timestamp_ns() > 0);
            assert_ne!(Some(lease.id()), last_id, "identity repeated");
            last_id = Some(lease.id());
            let mapped = lease.read().expect("mapped for the CPU").plane(0).is_some();
            assert!(mapped);
            frames.release(lease);
            seen += 1;
        }
    }
    assert!(seen >= 12, "frames seen: {seen}");
    // Every frame delivered was one acquire of the pool: the probe-free sweep adds none.
    assert_eq!(
        frames.stats().acquired,
        seen,
        "pool acquires vs frames delivered"
    );

    for counter in copy_counters() {
        assert_eq!(counter.copies(), 0, "{} copied", counter.site());
    }
    drop(frames);
    drop(sensor);
    drop(receiver);
    drop(isp);
    drop(client);
    service.stop();
}

#[test]
fn compressed_stream_leases_are_one_plane_of_each_frames_bytes() {
    let (service, socket) = start("compressed", &sim::board(Some("csi0")));
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

    // A compressed code: each frame's lease is one plane, as long as that frame's bytes.
    let mut frames: Frames = open_frames(
        &client,
        chain.receiver.index,
        "frames",
        FourCc::MJPG,
        layout.width,
        layout.height,
    )
    .expect("open compressed frame stream");

    receiver.start().expect("receiver start");
    sensor.start().expect("sensor start");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = 0usize;
    let mut lengths = std::collections::BTreeSet::new();
    while seen < 12 && Instant::now() < deadline {
        if let Some(lease) = frames.next(Duration::from_millis(200)).expect("next") {
            // The sim's identity is its frame sequence, and its frame bytes follow from that.
            let expect = sim::frame_bytes(layout.size, lease.id()) as usize;
            {
                let read = lease.read().expect("mapped for the CPU");
                let plane = read.plane(0).expect("one plane");
                assert_eq!(plane.data.len(), expect, "frame {} length", lease.id());
                assert!(read.plane(1).is_none(), "a compressed frame has one plane");
                lengths.insert(plane.data.len());
            }
            frames.release(lease);
            seen += 1;
        }
    }
    assert!(seen >= 12, "frames seen: {seen}");
    assert!(
        lengths.len() > 1,
        "frame lengths did not differ: {lengths:?}"
    );
    assert_eq!(
        frames.stats().acquired,
        seen,
        "pool acquires vs frames delivered"
    );
    assert_eq!(frames.oversize_drops(), 0);

    for counter in copy_counters() {
        assert_eq!(counter.copies(), 0, "{} copied", counter.site());
    }
    drop(frames);
    drop(sensor);
    drop(receiver);
    drop(client);
    service.stop();
}
