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
        cpu_access: CpuAccess::Cached,
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
        roi: None,
        regions: Vec::new(),
        overview: None,
        hardware_overview: false,
        unmet: vec![crate::planner::Unmet::Size {
            wanted: (160, 90),
            delivered: (1280, 800),
        }],
    };
    let messages = [
        ("frame", encode_frame(1, &frame)),
        ("frame-hops", {
            let mut with_hops = WireFrame {
                meta: frame.meta.clone(),
                layouts: frame.layouts.clone(),
                backing: WireBacking::Memfd { len: 512 },
                cpu_access: CpuAccess::Cached,
                companions: Vec::new(),
            };
            let hops = &mut with_hops.meta.hops;
            hops.set_sequence(Some(7));
            hops.set(Hop::Sensor, 1_000);
            hops.set(Hop::Queued, 5_000);
            hops.copied(64);
            encode_frame(2, &with_hops)
        }),
        (
            "release-hops",
            release_bytes(
                3,
                ClientHops {
                    received: Some(6_000),
                    imported: Some(6_500),
                },
            )
            .to_vec(),
        ),
        ("release", encode_release(1)),
        ("accept", encode_accept("plan", &delivered, None)),
        (
            "accept-token",
            encode_accept("plan", &delivered, Some(ClientToken { id: 3, token: 99 })),
        ),
        ("reject", encode_reject("busy")),
        ("roi", encode_roi(&[FrameRect::new(1, 2, 3, 4)])),
        ("list", encode_list()),
        ("cameras", encode_cameras(&cameras)),
        ("request", encode_request(&request, Some("cam"))),
        (
            "gray",
            encode_request(&crate::planner::Frames::gray().every_frame(2), None),
        ),
        ("any", encode_request(&crate::planner::Frames::any(), None)),
    ];
    let messages = messages.into_iter().chain(control_seeds());
    for (name, bytes) in messages {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
}

/// Control requests, replies, events and a control list (`wire/controls.rs`).
fn control_seeds() -> Vec<(&'static str, Vec<u8>)> {
    use super::controls::*;
    use super::wire::*;
    let request = |op| ControlRequest {
        seq: 7,
        camera: Some("cam".into()),
        token: Some(42),
        op,
    };
    let applied = AppliedControl {
        id: ControlId(0xF400_0001),
        requested: ControlValue::Uint(100_000),
        value: ControlValue::Uint(33_000),
        clamped: true,
        frame: Some(1234),
        deferred: false,
        restarted: false,
    };
    let rect = ControlRect {
        x: 1,
        y: 2,
        width: 30,
        height: 40,
    };
    let list = vec![ControlDescriptor {
        meta: ControlMeta {
            id: ControlId(9),
            name: "ae_flicker_mode".into(),
            kind: ControlKind::Menu,
            access: Access::ReadWrite,
            min: ControlValue::Int(0),
            max: ControlValue::Int(3),
            default: ControlValue::Int(3),
            step: Some(ControlValue::Int(1)),
            menu: Some(vec!["off".into(), "50".into(), "60".into(), "auto".into()]),
            metadata: ControlMetadata::default(),
        },
        current: Some(ControlValue::Int(1)),
        standard: Some(StandardControl::AeEnable),
        writable: true,
    }];
    vec![
        (
            "control-set",
            encode_control(&request(ControlOp::Set(
                StandardControl::ExposureUs.into(),
                ControlValue::Uint(1000),
            ))),
        ),
        (
            "control-set-rects",
            encode_control(&request(ControlOp::Set(
                ControlId(0xF400_0024).into(),
                ControlValue::Rects(vec![rect, rect]),
            ))),
        ),
        (
            "control-get",
            encode_control(&request(ControlOp::Get(ControlId(5).into()))),
        ),
        ("control-list", encode_control(&request(ControlOp::List))),
        (
            "control-subscribe",
            encode_control(&request(ControlOp::Subscribe)),
        ),
        (
            "control-applied",
            encode_control_reply(7, &ControlReply::Applied(applied)),
        ),
        (
            "control-value",
            encode_control_reply(
                7,
                &ControlReply::Value(ControlId(1), ControlValue::Rect(rect)),
            ),
        ),
        (
            "control-refused",
            encode_control_reply(
                7,
                &ControlReply::Refused(ControlRefusal::NotPermitted("owner only".into())),
            ),
        ),
        (
            "control-list-reply",
            encode_control_reply(7, &ControlReply::List(1, 64)),
        ),
        (
            "control-event",
            encode_control_event(&ControlEvent {
                id: ControlId(0xF400_0002),
                standard: Some(StandardControl::Gain),
                value: ControlValue::Float(2.5),
                frame: Some(10),
                by: Some(1),
            }),
        ),
        ("control-list-body", encode_control_list(&list)),
    ]
}

#[test]
fn control_messages_survive_the_wire() {
    use super::wire::*;
    for (name, bytes) in control_seeds() {
        let decoded = decode_client(&bytes).is_ok()
            || decode_server(&bytes).is_ok()
            || decode_control_list(&bytes).is_ok_and(|l| l.len() == 1);
        assert!(decoded, "{name} does not decode");
        // The fuzz target's round trips hold for them.
        super::fuzz_messages(&bytes);
    }
    let (_, body) = control_seeds().pop().unwrap();
    let list = decode_control_list(&body).unwrap();
    assert_eq!(list[0].meta.menu.as_ref().unwrap().len(), 4);
    assert_eq!(list[0].current, Some(ControlValue::Int(1)));
}

/// A dma-buf as a camera's would be: exported as dma-buf planes (here a memfd stands in for
/// the buffer), cached or not as it says.
struct CameraBuffer {
    fd: OwnedFd,
    map: Vec<u8>,
    access: CpuAccess,
}

impl ExternalBacking for CameraBuffer {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        (index == 0).then_some(self.map.as_slice())
    }

    fn residency(&self) -> FrameResidency {
        FrameResidency::Dmabuf
    }

    fn cpu_access(&self) -> CpuAccess {
        self.access
    }

    fn can_export(&self) -> bool {
        true
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        Ok(Some(FrameBackingExport::DmabufPlanes {
            planes: vec![FrameFdPlane {
                fd: self.fd.try_clone().map_err(FrameExportError::Fd)?,
                offset: 0,
                len: 512,
            }],
        }))
    }
}

#[test]
fn received_frames_read_as_cached_as_the_senders_memory() {
    let path = socket_path("cpu-access");
    let server = FrameServer::bind(&path).unwrap().max_in_flight(4);
    let client = FrameClient::connect(&path).unwrap();
    for access in [CpuAccess::Cached, CpuAccess::Uncached] {
        let source = memfd_frame();
        let fd = match source.export_backing().unwrap() {
            FrameBackingExport::Memfd { fd, .. } => fd,
            FrameBackingExport::DmabufPlanes { .. } => unreachable!(),
        };
        let frame = FrameLease::from_external(
            meta(64, 8),
            smallvec![PlaneLayout {
                offset: 0,
                len: 512,
                stride: 64,
            }],
            std::sync::Arc::new(CameraBuffer {
                fd,
                map: vec![7; 512],
                access,
            }),
        );
        assert_eq!(server.publish(&frame).unwrap(), 1);
        let got = recv(&client);
        assert_eq!(got.residency(), FrameResidency::Dmabuf);
        assert_eq!(got.cpu_access(), access);
        assert!(got.can_read_planes());
        assert_eq!(got.planes()[0].data()[0], 7);
    }
    // Copied on the way (heap frames): the copy is cached host memory.
    server.publish(&heap_frame(1)).unwrap();
    assert_eq!(recv(&client).cpu_access(), CpuAccess::Cached);
}
