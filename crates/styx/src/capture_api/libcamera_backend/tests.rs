use super::*;

#[test]
fn record_worker_error_keeps_last_failure_without_camera_hardware() {
    let worker_error = Mutex::new(None);
    let err = CaptureError::Backend("request loop failed".into());

    record_worker_error(&worker_error, &err);

    let stored = worker_error.lock().clone();
    assert_eq!(
        stored.as_ref().map(ToString::to_string),
        Some(err.to_string())
    );
}

/// libcamera names formats by DRM fourcc: DRM `XR24` (`XRGB8888`, bytes B, G, R, x) is Styx
/// (V4L2) `XR24`, DRM `RG24` (`RGB888`, bytes B, G, R) is Styx `BG24`, and so on.
#[test]
fn libcamera_formats_map_by_memory_layout() {
    use libcamera::pixel_format::PixelFormat;
    let lc = |code: &[u8; 4]| PixelFormat::new(u32::from_le_bytes(*code), 0);
    for (drm, styx) in [
        (b"XR24", FourCc::XR24),
        (b"XB24", FourCc::XB24),
        (b"AR24", FourCc::BGRA),
        (b"AB24", FourCc::RGBA),
        (b"RG24", FourCc::BG24),
        (b"BG24", FourCc::RG24),
        (b"NV12", FourCc::NV12),
        (b"YUYV", FourCc::YUYV),
    ] {
        assert_eq!(map_pixel_format_to_fourcc(lc(drm)), styx, "{styx}");
        assert_eq!(
            normalize_requested_fourcc_for_libcamera(styx),
            FourCc::new(*drm),
            "{styx}"
        );
    }
    assert_eq!(
        normalize_requested_fourcc_for_libcamera(FourCc::RGB3),
        FourCc::new(*b"BG24")
    );
    // Raw formats keep their Styx code in requests.
    let pbaa = FourCc::new(*b"pBAA");
    assert_eq!(normalize_requested_fourcc_for_libcamera(pbaa), pbaa);
    assert!(util::is_rgb24_request(FourCc::RG24) && util::is_rgb24_request(FourCc::BGR3));
    assert!(!util::is_rgb24_request(FourCc::XR24));
}

/// Only formats libcamera offers for a stream are asked for: matched by memory layout, a raw
/// request taking the offered format with its packing modifier, an unoffered one none (PiSP
/// aborted the process validating `AB24`, Styx `RGBA`, which it does not offer).
#[test]
fn only_offered_formats_are_requested() {
    use libcamera::pixel_format::PixelFormat;
    use styx_core::format::drm::MIPI_FORMAT_MOD_CSI2_PACKED;
    let lc = |code: &[u8; 4], modifier| PixelFormat::new(u32::from_le_bytes(*code), modifier);
    // What PiSP offers on a processed stream, and on the raw one.
    let pisp = [
        lc(b"YUYV", 0),
        lc(b"NV12", 0),
        lc(b"NV21", 0),
        lc(b"YU12", 0),
        lc(b"RG24", 0),
        lc(b"BG24", 0),
        lc(b"BG10", MIPI_FORMAT_MOD_CSI2_PACKED),
    ];
    let pick = |styx: FourCc| {
        streams::pick_offered(normalize_requested_fourcc_for_libcamera(styx), pisp)
            .map(|pf| (pf.fourcc(), pf.modifier()))
    };
    let drm = |code: &[u8; 4]| u32::from_le_bytes(*code);
    assert_eq!(pick(FourCc::NV12), Some((drm(b"NV12"), 0)));
    // Styx RG24 (bytes R, G, B) is DRM BG24.
    assert_eq!(pick(FourCc::RG24), Some((drm(b"BG24"), 0)));
    assert_eq!(pick(FourCc::RGBA), None);
    assert_eq!(pick(FourCc::BGRA), None);
    assert_eq!(pick(FourCc::XR24), None);
    assert_eq!(
        pick(FourCc::new(*b"pBAA")),
        Some((drm(b"BG10"), MIPI_FORMAT_MOD_CSI2_PACKED))
    );
    assert!(
        util::pisp_disallowed_fourcc(FourCc::RGBA) && util::pisp_disallowed_fourcc(FourCc::BGRA)
    );
}

#[test]
fn sensor_timestamps_clock() {
    use frame::sensor_clock;
    const S: u64 = 1_000_000_000;
    // Never suspended: the clocks agree to well under a frame; libcamera's documented clock.
    assert_eq!(
        sensor_clock(100 * S - 8_000_000, 100 * S, 100 * S + 3_000),
        TimestampClock::Boottime
    );
    // Suspended for a minute: a timestamp behind the monotonic clock is on it ...
    assert_eq!(
        sensor_clock(100 * S - 8_000_000, 100 * S, 160 * S),
        TimestampClock::Monotonic
    );
    // ... one ahead of it can only be boottime.
    assert_eq!(
        sensor_clock(160 * S - 8_000_000, 100 * S, 160 * S),
        TimestampClock::Boottime
    );
}

/// A memfd standing in for a dma-buf: `len` bytes, every byte `fill`.
fn memfd(len: usize, fill: u8) -> std::os::fd::OwnedFd {
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::FileExt;
    // SAFETY: creating an anonymous memory file; a non-negative result is ours.
    let fd = unsafe { libc::memfd_create(c"styx-libcamera-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: `fd` was just created.
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let file = std::fs::File::from(fd.try_clone().unwrap());
    file.set_len(len as u64).unwrap();
    file.write_all_at(&vec![fill; len], 0).unwrap();
    fd
}

fn handle(
    rx: styx_core::queue::BoundedRx<FrameLease>,
    live: crate::metrics::CaptureMetrics,
) -> CaptureHandle {
    CaptureHandle {
        backend: BackendKind::Libcamera,
        control: ControlPlane::Virtual,
        descriptor: CaptureDescriptor::new([]),
        mode: Mode::new(MediaFormat::srgb(FourCc::NV12, 64, 32).expect("format")),
        interval: None,
        rx,
        stop_tx: None,
        worker: None,
        aux_workers: Vec::new(),
        libcamera_idle_stop_allowed: false,
        libcamera_stop_when_idle: false,
        metrics: Default::default(),
        external_backings: Vec::new(),
        worker_error: Default::default(),
        control_error: Default::default(),
        shutdown_stats: Default::default(),
        retry_metrics: Default::default(),
        sequence_gaps: Default::default(),
        live,
    }
}

/// The libcamera frame path in the capturing process, without a camera: completed "requests"
/// (numbers standing in for libcamera's) over NV12 buffers whose two planes are on duplicates
/// of one fd, as libcamera hands them out, made into frames as the worker makes them,
/// delivered, taken, read and dropped (the request back to the worker), over many frames.
/// Each frame allocates its backing (the lease record) and nothing else (the buffers' mappings
/// are cached: see `planes_on_duplicated_fds_share_one_mapping`); the frames carry the
/// sensor-to-taken hops.
#[test]
#[allow(clippy::print_stdout)]
fn frames_carry_hops_and_allocate_only_their_lease() {
    use std::os::fd::AsRawFd;
    use std::sync::atomic::AtomicUsize;

    use crate::test_alloc::allocations;
    use frame::{Completed, frame_meta};

    const W: usize = 64;
    const H: usize = 32;
    const BUFFERS: usize = 4;
    const FRAMES: u64 = 400;
    let len = W * H * 3 / 2;
    // Each buffer: one memfd, a duplicate for each plane (libcamera's own fds).
    let buffers: Vec<_> = (0..BUFFERS)
        .map(|i| {
            let fd = memfd(len, i as u8 + 1);
            let planes = [fd.try_clone().unwrap(), fd.try_clone().unwrap()];
            (fd, planes)
        })
        .collect();
    let views = |i: usize| -> smallvec::SmallVec<[backing::BackingPlaneView; 3]> {
        let (_, planes) = &buffers[i];
        smallvec::smallvec![
            backing::BackingPlaneView {
                fd: planes[0].as_raw_fd(),
                offset: 0,
                len: W * H,
            },
            backing::BackingPlaneView {
                fd: planes[1].as_raw_fd(),
                offset: W * H,
                len: W * H / 2,
            },
        ]
    };
    let layouts: smallvec::SmallVec<[PlaneLayout; 3]> = smallvec::smallvec![
        PlaneLayout {
            offset: 0,
            len: W * H,
            stride: W,
        },
        PlaneLayout {
            offset: 0,
            len: W * H / 2,
            stride: W,
        },
    ];
    let format = MediaFormat::srgb(FourCc::NV12, W as u32, H as u32).expect("format");

    let live = crate::metrics::CaptureMetrics::default();
    let (queue_tx, queue_rx) = styx_core::queue::bounded(2);
    let handle = handle(queue_rx, live.clone());
    let shutting_down = Arc::new(AtomicBool::new(false));
    let outstanding = Arc::new(AtomicUsize::new(0));
    let slots: Vec<Arc<RequestSlot<usize>>> = (0..BUFFERS)
        .map(|_| RequestSlot::new(shutting_down.clone(), outstanding.clone()))
        .collect();
    let cache = Arc::new(backing::MappingCache::default());
    let tracker = || Arc::new(ExternalBackingTracker::new("test"));
    let (outstanding_tracker, mapped_tracker) = (tracker(), tracker());
    let mut readback = HashMap::new();
    let mut free: Vec<usize> = (0..BUFFERS).collect();
    let (mut lease_allocs, mut path_allocs, mut xor) = (0u64, 0u64, 0u8);
    // A warm-up as long as the native path's (the capture's metrics windows fill meanwhile).
    for n in 0..FRAMES + 100 {
        let measured = n >= 100;
        // The worker: requests back from consumers, one completes.
        let ((), returned) = allocations(|| {
            free.extend(slots.iter().filter_map(|slot| slot.take_returned()));
        });
        let req = free.pop().expect("a free request");
        let dequeued = std::time::Instant::now();
        let sensor = TimestampClock::Monotonic.now_ns().unwrap() - 9_000_000;
        // Its metadata (as libcamera reports it), then the frame.
        let ts = [sensor as i64];
        let metadata = [
            (
                libcamera::controls::ControlId::SensorTimestamp as u32,
                lcraw::RawValue::Int64(&ts),
            ),
            (
                libcamera::controls::ControlId::FrameDuration as u32,
                lcraw::RawValue::Int64(&[33_333]),
            ),
            (1, lcraw::RawValue::Float(&[1.5, 1.9])),
        ];
        let (frame, leased) = allocations(|| {
            let timing = lcraw::record_metadata(metadata, &mut readback);
            let completed = Completed {
                timestamp: timing.sensor_timestamp.unwrap(),
                clock: TimestampClock::Monotonic,
                conversion: None,
                sequence: n as u32,
                buffer_memory: "dma-heap",
                dequeued,
            };
            let backing = LibcameraBacking::new(
                &slots[req],
                req,
                views(req),
                cache.clone(),
                outstanding_tracker.clone(),
                mapped_tracker.clone(),
                true,
            );
            FrameLease::from_external(frame_meta(format, &completed), layouts.clone(), backing)
        });
        let (closed, delivered) = allocations(|| {
            deliver(
                &live,
                &queue_tx,
                frame,
                "libcamera",
                Duration::from_millis(100),
            )
        });
        assert!(!closed);
        // The consumer: take it, read every plane, drop it (the request goes back).
        let (hops, consumed) = allocations(|| {
            let RecvOutcome::Data(frame) = handle.recv() else {
                panic!("no frame");
            };
            for plane in frame.planes() {
                assert!(plane.data().iter().all(|&b| b == req as u8 + 1));
                xor ^= plane.data()[0];
            }
            let hops = frame.meta().hops;
            drop(frame);
            hops
        });
        if measured {
            lease_allocs += leased;
            path_allocs += returned + delivered + consumed;
        }
        for hop in [Hop::Sensor, Hop::Dequeued, Hop::Queued, Hop::Taken] {
            assert!(hops.get(hop).is_some(), "frame {n} has no {hop:?} hop");
        }
        assert_eq!(hops.get(Hop::Sensor), Some(sensor));
    }
    std::hint::black_box(xor);
    println!(
        "in-process libcamera path, allocations per frame: request to lease {:.2}, the rest {:.2}",
        lease_allocs as f64 / FRAMES as f64,
        path_allocs as f64 / FRAMES as f64
    );
    assert_eq!(path_allocs, 0, "the frame path allocates");
    assert_eq!(
        lease_allocs, FRAMES,
        "a frame allocates more than its lease record"
    );
    assert_eq!(
        outstanding.load(Ordering::Acquire),
        0,
        "requests not returned"
    );
    let path = handle.camera_metrics().path;
    let names: Vec<_> = path.hops.iter().map(|h| h.to.as_str()).collect();
    assert_eq!(names, ["dequeued", "queued", "taken"]);
    assert!(path.total.p50_ms.is_some());
}

/// A buffer whose planes are on two duplicates of its fd (as libcamera hands them out) is
/// mapped once, so a read syncs it once, not once per plane.
#[test]
fn planes_on_duplicated_fds_share_one_mapping() {
    use std::os::fd::AsRawFd;
    let fd = memfd(8192, 7);
    let (a, b) = (fd.try_clone().unwrap(), fd.try_clone().unwrap());
    let planes: smallvec::SmallVec<[backing::BackingPlaneView; 3]> = smallvec::smallvec![
        backing::BackingPlaneView {
            fd: a.as_raw_fd(),
            offset: 0,
            len: 4096
        },
        backing::BackingPlaneView {
            fd: b.as_raw_fd(),
            offset: 4096,
            len: 2048
        },
    ];
    assert_eq!(backing::mappings_for(&planes), Some(1));
    // Two different buffers stay two mappings.
    let other = memfd(4096, 1);
    let planes: smallvec::SmallVec<[backing::BackingPlaneView; 3]> = smallvec::smallvec![
        backing::BackingPlaneView {
            fd: a.as_raw_fd(),
            offset: 0,
            len: 4096
        },
        backing::BackingPlaneView {
            fd: other.as_raw_fd(),
            offset: 0,
            len: 4096
        },
    ];
    assert_eq!(backing::mappings_for(&planes), Some(2));
}
