use std::os::fd::{FromRawFd, OwnedFd};
use std::time::Duration;

use smallvec::smallvec;
use styx_core::prelude::*;

use super::{FrameClient, FrameServer};

fn socket_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("styx-ipc-{name}-{}.sock", std::process::id()))
}

fn meta(width: u32, height: u32) -> FrameMeta {
    let res = Resolution::new(width, height).unwrap();
    let mut meta = FrameMeta::new(MediaFormat::new(FourCc::GREY, res, ColorSpace::Srgb), 42);
    meta.clock = Some(TimestampClock::Monotonic);
    meta
}

/// A 64x8 grey frame on the heap.
fn heap_frame(seed: u8) -> FrameLease {
    let mut buf = BufferPool::with_limits(1, 512, 1).lease();
    buf.resize(512);
    for (i, px) in buf.as_mut_slice().iter_mut().enumerate() {
        *px = (i as u8).wrapping_add(seed);
    }
    FrameLease::single_plane(meta(64, 8), buf, 512, 64)
}

/// A 64x8 grey frame in a memfd (shareable without copying).
fn memfd_frame() -> FrameLease {
    // SAFETY: creating an anonymous memory file; a non-negative result is ours.
    let fd = unsafe { libc::memfd_create(c"styx-ipc-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: `fd` was just created.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let file = std::fs::File::from(fd.try_clone().unwrap());
    std::os::unix::fs::FileExt::write_all_at(&file, &[7u8; 512], 0).unwrap();
    let layout = PlaneLayout {
        offset: 0,
        len: 512,
        stride: 64,
    };
    FrameLease::from_memfd(meta(64, 8), smallvec![layout], fd)
}

fn recv(client: &FrameClient) -> FrameLease {
    match client.recv(Duration::from_secs(2)) {
        RecvOutcome::Data(frame) => frame,
        other => panic!("no frame: {}", matches!(other, RecvOutcome::Closed)),
    }
}

#[test]
fn clients_get_frames_and_release_them() {
    let path = socket_path("release");
    let server = FrameServer::bind(&path).unwrap().max_in_flight(2);
    let client = FrameClient::connect(&path).unwrap();

    assert_eq!(server.publish(&heap_frame(3)).unwrap(), 1);
    let frame = recv(&client);
    assert_eq!(frame.meta().timestamp, 42);
    assert_eq!(frame.meta().clock, Some(TimestampClock::Monotonic));
    assert_eq!(frame.meta().format.code, FourCc::GREY);
    let rows = frame.luma_rows().unwrap();
    assert_eq!(rows.row(1).unwrap().data()[0], 64u8.wrapping_add(3));
    // Heap frames are copied once; memfd frames are passed as they are.
    assert_eq!(server.stats().copied, 1);
    server.publish(&memfd_frame()).unwrap();
    let shared = recv(&client);
    assert_eq!(shared.planes()[0].data()[0], 7);
    assert_eq!(server.stats().copied, 1);

    // Holding two frames: the next one is skipped until one is dropped.
    assert_eq!(server.publish(&heap_frame(0)).unwrap(), 0);
    assert_eq!(server.stats().skipped, 1);
    drop(frame);
    assert_eq!(server.publish(&heap_frame(0)).unwrap(), 1);
    drop((shared, recv(&client)));

    drop(server);
    assert!(matches!(
        client.recv(Duration::from_secs(1)),
        RecvOutcome::Closed
    ));
    assert!(!path.exists());
}

#[test]
fn a_client_that_leaves_is_forgotten() {
    let path = socket_path("leave");
    let server = FrameServer::bind(&path).unwrap();
    let staying = FrameClient::connect(&path).unwrap();
    let leaving = FrameClient::connect(&path).unwrap();
    assert_eq!(server.publish(&heap_frame(0)).unwrap(), 2);
    drop(leaving);
    assert_eq!(server.publish(&heap_frame(1)).unwrap(), 1);
    assert_eq!(server.stats().clients, 1);
    drop((recv(&staying), recv(&staying)));
}

#[test]
fn a_client_holding_a_frame_too_long_is_disconnected() {
    let path = socket_path("max-hold");
    let server = FrameServer::bind(&path)
        .unwrap()
        .max_hold(Some(Duration::from_millis(50)));
    let client = FrameClient::connect(&path).unwrap();
    server.publish(&memfd_frame()).unwrap();
    let held = recv(&client);
    std::thread::sleep(Duration::from_millis(100));
    // Its frame is taken back and it gets nothing more.
    assert_eq!(server.publish(&memfd_frame()).unwrap(), 0);
    assert_eq!((server.stats().revoked, server.stats().clients), (1, 0));
    assert!(matches!(
        client.recv(Duration::from_secs(1)),
        RecvOutcome::Closed
    ));
    drop(held);
}

/// Writes one of every message a service or client reads to `$STYX_FUZZ_SEEDS/ipc_messages/`,
/// as seeds for the `ipc_messages` and `ipc_request` fuzz targets (`docs/fuzzing.md`).
#[test]
#[ignore = "writes fuzz seeds; run with STYX_FUZZ_SEEDS set"]
fn write_fuzz_seeds() {
    use super::wire::*;
    let Some(dir) = std::env::var_os("STYX_FUZZ_SEEDS") else {
        return;
    };
    let dir = std::path::Path::new(&dir).join("ipc_messages");
    std::fs::create_dir_all(&dir).unwrap();
    let frame = WireFrame {
        meta: meta(64, 8),
        layouts: vec![PlaneLayout {
            offset: 0,
            len: 512,
            stride: 64,
        }],
        backing: WireBacking::Memfd { len: 512 },
        companions: Vec::new(),
    };
    let request = crate::planner::Frames::formats([FourCc::NV12, FourCc::MJPG])
        .size(320, 180)
        .fps_between(15, 30)
        .pyramid(2)
        .roi(FrameRect::new(1, 2, 3, 4))
        .forbid("ffmpeg");
    let cameras = [CameraInfo {
        name: "cam".into(),
        keys: vec!["native:ov9782".into()],
        in_use: false,
    }];
    let delivered = crate::planner::Delivered {
        format: FourCc::GREY,
        size: (1280, 800),
        fps: Some(60.0),
        pyramid_levels: 1,
        hardware_pyramid_level: Some(1),
        inter_coded: false,
        unmet: vec![crate::planner::Unmet::Size {
            wanted: (160, 90),
            delivered: (1280, 800),
        }],
    };
    let messages = [
        ("frame", encode_frame(1, &frame)),
        ("release", encode_release(1)),
        ("accept", encode_accept("plan", &delivered)),
        ("reject", encode_reject("busy")),
        ("roi", encode_roi(Some(FrameRect::new(1, 2, 3, 4)))),
        ("list", encode_list()),
        ("cameras", encode_cameras(&cameras)),
        ("request", encode_request(&request, Some("cam"))),
        (
            "gray",
            encode_request(&crate::planner::Frames::gray().every_frame(2), None),
        ),
        ("any", encode_request(&crate::planner::Frames::any(), None)),
    ];
    for (name, bytes) in messages {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
}
