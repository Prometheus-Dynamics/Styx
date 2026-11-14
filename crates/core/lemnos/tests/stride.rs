//! The frame stream's plane layout comes from the receiver: a receiver that pads its rows gives
//! each lease that stride, and row `r` of the frame starts at `r * stride`.
//!
//! The width is chosen so that the packed row (100 pixels at 10 bits is 125 bytes) is not what the
//! receiver writes: its layout pads each row to 128 bytes. Packed rows would start at 125.

#[allow(dead_code)]
use styx_sim::board::{Board, Family};
use styx_sim::drivers::{SimIsp, SimReceiver, SimSensor};

use std::path::PathBuf;
use std::time::{Duration, Instant};

use lemnos_contract_camera_receiver::{CameraReceiver, LinkConfig, Packing};
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_format::FourCc;
use styx_lemnos::{Client, Frames, Geometry, open_frames_writable_at};

const WIDTH: u32 = 100;
const HEIGHT: u32 = 8;
const PACKED_ROW: u32 = 125;

fn start(name: &str) -> (Service, PathBuf) {
    let dir = std::env::temp_dir()
        .join(format!("styx-lemnos-stride-{}", std::process::id()))
        .join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("sock");
    let config = Config {
        socket: socket.clone(),
        name: "styx-lemnos-test".to_string(),
        board: parse(&Board::timed(Family::Lemnos).text()).expect("board"),
        drivers: vec![
            entry::<SimSensor>(),
            entry::<SimReceiver>(),
            entry::<SimIsp>(),
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
fn the_receivers_stride_is_the_leases_stride_and_row_r_starts_at_r_times_stride() {
    let (service, socket) = start("padded");
    let client = Client::connect(&socket, "styx-lemnos-test").expect("connect");
    let devices = client.list().expect("list");
    let receiver_index = devices
        .iter()
        .find(|d| d.name == "csi0")
        .expect("csi0")
        .index;

    let mut receiver = client
        .open::<dyn CameraReceiver>("csi0")
        .expect("receiver contract");
    let link = LinkConfig {
        lanes: 2,
        virtual_channel: 0,
        data_type: 0x2b,
        bit_depth: 10,
        continuous_clock: false,
        width: WIDTH,
        height: HEIGHT,
        pixel_rate_hz: 1_536_000,
        link_rate_mbps: 1000,
    };
    let layout = receiver
        .configure(link, Packing::Packed)
        .expect("configure receiver");
    assert_eq!(layout.stride, 128, "the receiver pads 125-byte rows to 128");
    assert_eq!(layout.size, 128 * HEIGHT);
    assert_eq!(receiver.layout().expect("layout"), Some(layout));

    let geometry = Geometry {
        code: FourCc::RGGB10P,
        width: layout.width,
        height: layout.height,
        stride: layout.stride,
    };
    let mut frames: Frames = open_frames_writable_at(&client, receiver_index, "frames", geometry)
        .expect("open padded frame stream");
    receiver.start().expect("receiver start");

    let stride = layout.stride as usize;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut checked = 0usize;
    while checked < 3 && Instant::now() < deadline {
        let Some(mut lease) = frames.next(Duration::from_millis(200)).expect("next") else {
            continue;
        };
        {
            let mut write = lease.write().expect("sole owner of a new frame");
            let plane = write.plane(0).expect("one plane");
            assert_eq!(plane.stride, stride, "lease stride is the receiver's");
            assert_eq!(plane.data.len(), stride * HEIGHT as usize);
            for row in 0..HEIGHT as usize {
                // Row r starts at r * stride. Its first byte is marked with its row number.
                plane.data[row * stride] = row as u8 + 1;
                plane.data[row * stride + 1] = 0xA5;
            }
        }
        {
            let read = lease.read().expect("mapped for the CPU");
            let plane = read.plane(0).expect("one plane");
            assert_eq!(plane.stride, stride);
            for row in 0..HEIGHT as usize {
                assert_eq!(plane.data[row * stride], row as u8 + 1, "row {row}");
                assert_eq!(plane.data[row * stride + 1], 0xA5, "row {row} second byte");
            }
            // The packed row would have started at 125: that byte is still the receiver's padding.
            assert_eq!(
                plane.data[PACKED_ROW as usize], 0,
                "padding after row 0's pixels"
            );
        }
        frames.release(lease);
        checked += 1;
    }
    assert!(checked >= 3, "frames checked: {checked}");

    receiver.stop().expect("receiver stop");
    frames.close().expect("close");
    drop(receiver);
    drop(client);
    service.stop();
}
