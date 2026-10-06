//! The Daedalus frame view paths of the zero-allocation proof (`daedalus` feature).

use super::*;

/// The bytes a Daedalus node reads through `FrameView`, sampled as [`read`] does: one CPU
/// access per plane, ended as each guard drops.
fn read_view(view: styx::core::daedalus::FrameView<'_>) -> u8 {
    view.cpu_planes()
        .flatten()
        .fold(0, |a, p| p.iter().step_by(1024).fold(a, |a, &b| a ^ b))
}

/// A camera service's client handing each frame to Daedalus code as its generic frame view:
/// borrowed straight from the lease (`frame_view`), and from a payload (`frame_payload`) retyped
/// by Styx's `daedalus:frame` provider, as the planner's `View` adapter does before a
/// `FrameView` port. The view reads the lease's own planes: nothing copied, and nothing
/// allocated per frame beyond the received frame's release record (a payload adds its own two,
/// the shared lease and its storage, with or without the view).
pub(super) fn camera_service_frame_view() {
    use styx::core::daedalus::{FrameInterface, frame_payload, frame_view};

    let res = Resolution::new(WIDTH, HEIGHT).unwrap();
    let mode = Mode::with_interval(
        MediaFormat::new(FourCc::NV12, res, ColorSpace::Srgb),
        Interval::from_fps(200).unwrap(),
    );
    let device = styx::capture_api::make_virtual_device("zero-alloc-view", [mode]);
    let path = socket_path("service-view");
    let _service = CameraService::new(device)
        .keep_streaming()
        .serve(&path)
        .unwrap();
    let client = FrameClient::request(&path, &Frames::nv12().size(WIDTH, HEIGHT)).unwrap();
    let next = |client: &FrameClient| loop {
        if let RecvOutcome::Data(frame) = client.recv(Duration::from_secs(2)) {
            return frame;
        }
    };
    let viewed = |frame: &FrameLease| {
        let view = frame_view(frame);
        assert_eq!(view.format(), u32::from_le_bytes(*b"NV12"));
        assert_eq!(view.plane_count() as usize, frame.layout_slice().len());
        // In place: the view's planes are the lease's.
        let luma = view.plane_bytes(0).expect("CPU-mapped");
        assert_eq!(luma.as_ptr(), frame.planes()[0].data().as_ptr());
        drop(luma);
        std::hint::black_box(read_view(view));
    };
    let through_payload = |frame: FrameLease| {
        let payload = frame_payload(frame)
            .provide_foreign::<FrameLease, FrameInterface>()
            .unwrap();
        let view = payload.foreign_borrow().unwrap().view::<FrameInterface>();
        std::hint::black_box(read_view(view.unwrap()));
    };
    IS_TEST.with(|t| t.set(true));
    for _ in 0..100 {
        viewed(&next(&client));
        through_payload(next(&client));
    }
    let copies_before = styx::metrics::path().total_copies();
    let per = |n: u64| n as f64 / FRAMES as f64;
    let counts = counted(|| {
        for _ in 0..FRAMES {
            viewed(&next(&client));
        }
    });
    println!(
        "camera service, FrameView of each frame: client {:.2} allocations per frame, service thread {:.2}",
        per(counts[Kind::Test as usize]),
        per(counts[Kind::ServiceClient as usize]),
    );
    // One refcounted record per received frame (its release), as without the view.
    assert!(
        counts[Kind::Test as usize] <= FRAMES,
        "receiving and viewing allocates {} times for {FRAMES} frames",
        counts[Kind::Test as usize]
    );
    assert_eq!(counts[Kind::ServiceClient as usize], 0);
    let mut receive = 0;
    let counts = counted(|| {
        for _ in 0..FRAMES {
            let frame = counted_here(&mut receive, || next(&client));
            through_payload(frame);
        }
    });
    println!(
        "camera service, FrameView through a payload: receive {:.2}, payload + view + release {:.2} allocations per frame",
        per(receive),
        per(counts[Kind::Test as usize] - receive),
    );
    assert!(receive <= FRAMES, "receiving allocates {receive} times");
    // `frame_payload`'s own two (the shared lease and the payload's storage); the provider
    // retypes the payload in place and the view borrows it.
    let payload = counts[Kind::Test as usize] - receive;
    assert!(
        payload <= 2 * FRAMES,
        "a payload and its view allocate {payload} times for {FRAMES} frames"
    );
    assert_eq!(
        styx::metrics::path().total_copies(),
        copies_before,
        "frames were copied"
    );
    assert_eq!(client.hop_metrics().copied, 0);
    IS_TEST.with(|t| t.set(false));
}

/// A dma-buf of `len` bytes from the system dma-heap, `None` without one (or access to it).
fn heap_dmabuf(len: usize) -> Option<OwnedFd> {
    /// `struct dma_heap_allocation_data`.
    #[repr(C)]
    struct Allocation {
        len: u64,
        fd: u32,
        fd_flags: u32,
        heap_flags: u64,
    }
    // _IOWR('H', 0, struct dma_heap_allocation_data)
    const DMA_HEAP_IOCTL_ALLOC: u64 = 0xc018_4800;
    let heap = std::fs::File::open("/dev/dma_heap/system").ok()?;
    let mut alloc = Allocation {
        len: len as u64,
        fd: 0,
        fd_flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32,
        heap_flags: 0,
    };
    // SAFETY: DMA_HEAP_IOCTL_ALLOC reads and writes one `dma_heap_allocation_data`.
    let r = unsafe {
        libc::ioctl(
            std::os::fd::AsRawFd::as_raw_fd(&heap),
            DMA_HEAP_IOCTL_ALLOC as _,
            &mut alloc,
        )
    };
    // SAFETY: on success the kernel returned a new dma-buf descriptor that we now own.
    (r == 0).then(|| unsafe { OwnedFd::from_raw_fd(alloc.fd as i32) })
}

/// What a dma-buf importer (a GPU) reads of a frame: geometry, format and each plane's fd,
/// offset, stride and length, never the bytes. The planes with an fd.
fn import_view(view: styx::core::daedalus::FrameView<'_>) -> usize {
    use styx::core::daedalus::{FrameFormatKind, PlaneMapping};

    std::hint::black_box((view.width(), view.height(), view.format(), view.modifier()));
    assert_eq!(view.format_kind(), FrameFormatKind::Pixel);
    let mut fds = 0;
    for plane in view.planes() {
        // A received dma-buf: readable, but how the sender's memory is cached is not known.
        assert_eq!(plane.mapping, PlaneMapping::Uncached);
        std::hint::black_box((plane.offset, plane.stride, plane.len));
        fds += usize::from(plane.dmabuf_fd.is_some());
    }
    fds
}

/// A frame socket's dma-buf frames (NV12, two planes on one buffer, as the PiSP's) through
/// Daedalus's frame view, borrowed (`frame_view`) and through a payload: a consumer that only
/// reads metadata and fds (a GPU importer) makes the received frames' backing map nothing,
/// sync nothing and begin no CPU access (`plane_data`); a CPU consumer begins one access per
/// plane and frame, syncs once on its first and once on its last, and maps each buffer once
/// (the receiver's mapping cache). No copies; nothing allocated beyond each fetched frame's
/// record (and a payload's own two).
pub(super) fn frame_socket_frame_view() {
    use styx::core::daedalus::{FrameInterface, frame_payload, frame_view};

    let path = socket_path("frame-socket-view");
    let socket = FrameSocket::bind(&path).unwrap();
    let heap: Option<Vec<OwnedFd>> = (0..6).map(|_| heap_dmabuf(LEN)).collect();
    let dma_heap = heap.is_some();
    let camera = Camera::over(heap.unwrap_or_else(|| (0..6).map(|_| memfd(LEN)).collect()));
    let mut fetcher = FrameFetcher::new(&path);
    let mut seq = 0;
    let mut next = |fetch: &mut u64| {
        seq += 1;
        let frame = camera.capture(seq).expect("a free buffer");
        socket.publish(&frame).unwrap();
        drop(frame);
        counted_here(fetch, || fetcher.fetch(Duration::from_secs(2)).unwrap())
    };
    let leases_end = |socket: &FrameSocket| {
        let t = std::time::Instant::now();
        while socket.stats().leases > 0 {
            assert!(t.elapsed() < Duration::from_secs(2));
            std::thread::yield_now();
        }
    };
    let fd_only = |frame: FrameLease, direct: &mut u64, payload: &mut u64| {
        let fds = counted_here(direct, || import_view(frame_view(&frame)));
        assert_eq!(fds, 2, "planes without their dma-buf");
        counted_here(payload, || {
            let payload = frame_payload(frame)
                .provide_foreign::<FrameLease, FrameInterface>()
                .unwrap();
            let view = payload.foreign_borrow().unwrap().view::<FrameInterface>();
            assert_eq!(import_view(view.unwrap()), 2);
        });
    };
    let p = styx::core::metrics::path_counters();
    let costs = || (p.frame_maps.get(), p.cpu_reads.get(), p.syncs.get());
    let delta = |before: (u64, u64, u64)| {
        let now = costs();
        (now.0 - before.0, now.1 - before.1, now.2 - before.2)
    };
    IS_TEST.with(|t| t.set(true));
    let (mut ignored, mut fetch, mut direct, mut payload) = (0, 0, 0, 0);
    for _ in 0..50 {
        let frame = next(&mut ignored);
        fd_only(frame, &mut direct, &mut payload);
        leases_end(&socket);
    }
    (direct, payload) = (0, 0);
    let copies_before = styx::metrics::path().total_copies();
    let before = costs();
    counted(|| {
        for _ in 0..FRAMES {
            let frame = next(&mut fetch);
            fd_only(frame, &mut direct, &mut payload);
            leases_end(&socket);
        }
    });
    let (maps, plane_data, syncs) = delta(before);
    let per = |n: u64| n as f64 / FRAMES as f64;
    println!(
        "frame socket ({}), fd-only FrameView consumer: {maps} mmaps, {plane_data} plane_data calls, {syncs} dma-buf syncs for {FRAMES} frames; allocations per frame: fetch + import {:.2}, view {:.2}, payload + view {:.2}",
        if dma_heap {
            "dma-heap dma-bufs"
        } else {
            "memfds as dma-bufs"
        },
        per(fetch),
        per(direct),
        per(payload),
    );
    assert_eq!(
        (maps, plane_data, syncs),
        (0, 0, 0),
        "an fd-only consumer made Styx map, begin CPU access or sync"
    );
    assert!(fetch <= FRAMES, "fetching allocates {fetch} times");
    assert_eq!(direct, 0, "a borrowed view allocates");
    assert!(
        payload <= 2 * FRAMES,
        "a payload and its view allocate {payload} times"
    );

    // A CPU consumer: both planes read at once, one access each.
    let before = costs();
    for _ in 0..FRAMES {
        let frame = next(&mut ignored);
        let view = frame_view(&frame);
        let (luma, chroma) = (view.plane_bytes(0).unwrap(), view.plane_bytes(1).unwrap());
        std::hint::black_box((luma[0], chroma[0]));
        drop((luma, chroma));
        drop(frame);
        leases_end(&socket);
    }
    let (maps, plane_data, syncs) = delta(before);
    println!(
        "frame socket, CPU FrameView consumer: {maps} mmaps, {plane_data} plane_data calls, {syncs} dma-buf syncs for {FRAMES} frames"
    );
    assert!(maps <= 6, "{maps} mmaps for 6 buffers");
    assert_eq!(plane_data, 2 * FRAMES);
    assert_eq!(syncs, 2 * FRAMES, "one START and one END per frame");
    assert_eq!(styx::metrics::path().total_copies(), copies_before);
    assert_eq!(fetcher.hop_metrics().copied, 0);
    IS_TEST.with(|t| t.set(false));
}
