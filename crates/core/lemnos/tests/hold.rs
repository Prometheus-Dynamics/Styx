//! The hold a frame stream's reader declares when it takes the stream: `consumer_hold` of the
//! stream's buffers, read from the host's description before anything is taken. On the simulated
//! camera, with the receiver's frame stream at 2, 4, 6 and 8 buffers.

use styx_sim::paced as sim;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use lemnos_contract_camera_receiver::{CameraReceiver, Packing};
use lemnos_contract_camera_sensor::CameraSensor;
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_format::FourCc;
use styx_lemnos::{Client, Error, Event, Frames, consumer_hold, find_chain, open_frames};

/// The simulated board with each receiver's frame stream at `buffers` buffers.
fn start(buffers: u32) -> (tempfile::TempDir, Service, PathBuf) {
    let text = sim::board(Some("csi0")).replace(
        "[devices.streams.frames]\nbuffers = 8\n",
        &format!("[devices.streams.frames]\nbuffers = {buffers}\n"),
    );
    assert!(text.contains(&format!("buffers = {buffers}\n")), "{text}");
    let dir = tempfile::Builder::new()
        .prefix("styx-hold-")
        .tempdir()
        .expect("a scratch folder");
    let socket = dir.path().join("sock");
    let config = Config {
        socket: socket.clone(),
        name: "styx-lemnos-test".to_string(),
        board: parse(&text).expect("board"),
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
    (dir, Service::start(config).expect("start lemnosd"), socket)
}

/// The receiver's frame stream taken as a reader, and the sensor and receiver to start it with.
struct Camera {
    client: Client,
    receiver_index: u32,
    width: u32,
    height: u32,
}

fn camera(socket: &PathBuf) -> Camera {
    let client = Client::connect(socket, "styx-lemnos-test").expect("connect");
    let devices = client.list().expect("list");
    let chain = find_chain(&devices).expect("chain");
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
    Camera {
        client,
        receiver_index: chain.receiver.index,
        width: layout.width,
        height: layout.height,
    }
}

fn take(camera: &Camera) -> Result<Frames, Error> {
    open_frames(
        &camera.client,
        camera.receiver_index,
        "frames",
        FourCc::RGGB10P,
        camera.width,
        camera.height,
    )
}

#[test]
fn the_policy_is_what_the_producer_keeps_half_and_at_least_two() {
    let holds: Vec<u32> = (0..=10).map(consumer_hold).collect();
    assert_eq!(holds, [0, 0, 0, 1, 2, 3, 3, 4, 4, 5, 5]);
}

#[test]
fn the_hold_is_declared_at_attach_for_4_6_and_8_buffers_and_frames_flow() {
    for (buffers, hold) in [(4, 2), (6, 3), (8, 4)] {
        let (_dir, service, socket) = start(buffers);
        let camera = camera(&socket);
        let mut frames = take(&camera).expect("the stream is taken with its hold");
        assert_eq!(frames.buffers(), buffers as usize);
        assert_eq!(frames.hold(), Some(hold), "{buffers} buffers");
        // Within the hold, frames flow: each is given back before the next.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = 0;
        while seen < 6 {
            assert!(
                Instant::now() < deadline,
                "{buffers} buffers: {seen} frames"
            );
            if let Some(lease) = frames.next(Duration::from_millis(200)).expect("next") {
                frames.release(lease);
                seen += 1;
            }
        }
        assert_eq!(frames.at_limit(), 0, "{buffers} buffers: never at the hold");
        drop(frames);
        drop(camera);
        service.stop();
    }
}

#[test]
fn a_reader_past_its_hold_reads_no_frame_and_counts_it_until_it_gives_one_back() {
    let (_dir, service, socket) = start(4);
    let camera = camera(&socket);
    let mut frames = take(&camera).expect("taken");
    assert_eq!(frames.hold(), Some(2));
    // Two leases kept: the reader holds all it declared.
    let mut kept = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while kept.len() < 2 {
        assert!(Instant::now() < deadline, "kept {}", kept.len());
        if let Some(lease) = frames.next(Duration::from_millis(200)).expect("next") {
            kept.push(lease);
        }
    }
    // Past its hold the reader is at the limit: no frame, the read is a timeout, and counted.
    let deadline = Instant::now() + Duration::from_secs(10);
    while frames.at_limit() == 0 {
        assert!(Instant::now() < deadline, "never at the limit");
        match frames.next_event(Duration::from_millis(50)).expect("event") {
            Event::TimedOut => {}
            Event::Frame(lease) => panic!("a frame past the hold: {}", lease.id()),
            other => panic!("{other:?}"),
        }
    }
    // One given back: frames flow again.
    kept.remove(0);
    let deadline = Instant::now() + Duration::from_secs(10);
    let lease = loop {
        assert!(Instant::now() < deadline, "no frame after giving one back");
        if let Some(lease) = frames.next(Duration::from_millis(200)).expect("next") {
            break lease;
        }
    };
    drop(lease);
    drop(kept);
    drop(frames);
    drop(camera);
    service.stop();
}

#[test]
fn a_stream_too_small_to_hold_one_is_refused_by_name_and_nothing_is_taken() {
    let (_dir, service, socket) = start(2);
    let camera = camera(&socket);
    match take(&camera) {
        Err(error @ Error::NoHold { .. }) => {
            assert!(
                matches!(&error, Error::NoHold { stream, buffers: 2 } if stream == "frames"),
                "{error:?}"
            );
            assert!(error.to_string().contains("stream frames"), "{error}");
        }
        Err(other) => panic!("refused for another reason: {other}"),
        Ok(frames) => panic!("taken with hold {:?}", frames.hold()),
    }
    drop(camera);
    service.stop();
}
