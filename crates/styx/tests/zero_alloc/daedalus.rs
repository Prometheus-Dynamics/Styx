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
        let payload = frame_payload(frame);
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
        per(counts[Kind::Test]),
        per(counts[Kind::ServiceClient]),
    );
    // One refcounted record per received frame (its release), as without the view (and a
    // buffer's mapping, if the capture's pool had to make one).
    counts.assert_maps_only_new_buffers();
    assert_eq!(
        counts[Kind::Test],
        FRAMES + counts.maps,
        "receiving and viewing allocates {} times for {FRAMES} frames ({} buffers mapped)",
        counts[Kind::Test],
        counts.maps
    );
    assert_eq!(counts[Kind::ServiceClient], 0);
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
        per(counts[Kind::Test] - receive),
    );
    assert_eq!(receive, FRAMES, "receiving allocates {receive} times");
    // `frame_payload`'s one (the shared lease's `Arc`, which `Payload::providing` stores as is);
    // the view borrows the lent handle (its read maps a new buffer).
    counts.assert_maps_only_new_buffers();
    let payload = counts[Kind::Test] - receive;
    assert_eq!(
        payload,
        FRAMES + counts.maps,
        "a payload and its view allocate {payload} times for {FRAMES} frames ({} buffers mapped)",
        counts.maps
    );
    assert_eq!(
        styx::metrics::path().total_copies(),
        copies_before,
        "frames were copied"
    );
    assert_eq!(client.hop_metrics().copied, 0);
    IS_TEST.with(|t| t.set(false));
}
