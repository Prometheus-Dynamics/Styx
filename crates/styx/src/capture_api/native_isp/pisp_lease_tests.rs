use std::time::{Duration, Instant};

use styx_pipeline::SensorValues;
use styx_pipeline::device::PispTimes;
use styx_pisp::device::BeJob;

use super::*;

/// A back end buffer stand-in: a memfd holding an NV12 frame (Y = 1, CbCr = 2).
pub(super) fn buffer(w: usize, h: usize) -> BeBuffer {
    let len = w * h * 3 / 2;
    let buf = DmaBuf::memfd("nv12", len).unwrap();
    let mut map = buf.map().unwrap();
    map.as_mut_slice()[..w * h].fill(1);
    map.as_mut_slice()[w * h..].fill(2);
    BeBuffer {
        map: Arc::new(map),
        fd: Arc::new(buf.into_fd()),
        cached: true,
    }
}

pub(super) fn frame() -> PispFrame {
    PispFrame {
        sequence: 7,
        timestamp: Duration::from_millis(5),
        dequeued: Instant::now(),
        sensor: SensorValues {
            frame: 7,
            exposure: Duration::from_millis(10),
            analogue_gain: 2.0,
            digital_gain: 1.0,
            frame_duration: Duration::from_millis(33),
            verified: true,
        },
        job: BeJob {
            outputs: [Some(3), Some(1)],
            elapsed: Duration::ZERO,
        },
        sequence_mismatch: false,
        request_lands: None,
        settings_from: Some(6),
        raw: None,
        raw_image: None,
        digital_gain: 1.0,
        flicker: 1.0,
        times: PispTimes::default(),
        passes: Vec::new(),
    }
}

#[test]
fn leases_carry_the_configured_clock() {
    let (w, h) = (64, 32);
    let spec = OutputSpec {
        code: FourCc::NV12,
        width: w as u32,
        height: h as u32,
    };
    let (tx, _rx) = returns();
    let live = CaptureMetrics::default();
    let placed = || Placed {
        spec,
        stride: w,
        size: None,
        crop: None,
    };
    let f = frame();
    let lease_in = |clock| {
        let conversion = crate::capture_api::native_backend::native_conversion(clock);
        lease(
            placed(),
            buffer(w, h),
            (0, 3),
            &f,
            &tx,
            &live,
            conversion,
            CpuPrep::LAZY,
            None,
            Default::default(),
        )
    };
    let native = lease_in(ClockSource::Native);
    assert_eq!(native.meta().clock, Some(TimestampClock::Monotonic));
    assert_eq!(native.meta().timestamp, f.timestamp.as_nanos() as u64);
    let boot = lease_in(ClockSource::Boottime);
    assert_eq!(boot.meta().clock, Some(TimestampClock::Boottime));
    let back = boot.meta().timestamp_in(TimestampClock::Monotonic).unwrap();
    assert!(
        back.abs_diff(f.timestamp.as_nanos() as u64) < 10_000_000,
        "{back}"
    );
}

#[test]
fn nv12_leases_export_their_planes_and_return_the_buffer() {
    let (w, h) = (64, 32);
    let spec = OutputSpec {
        code: FourCc::NV12,
        width: w as u32,
        height: h as u32,
    };
    let (tx, rx) = returns();
    let live = CaptureMetrics::default();
    let placed = Placed {
        spec,
        stride: w,
        size: None,
        crop: None,
    };
    let f = lease(
        placed,
        buffer(w, h),
        (0, 3),
        &frame(),
        &tx,
        &live,
        None,
        CpuPrep::LAZY,
        None,
        Default::default(),
    );
    let planes = f.planes();
    assert_eq!(planes.len(), 2);
    assert!(planes[0].data().iter().all(|&v| v == 1));
    assert_eq!(planes[1].data().len(), w * h / 2);
    assert!(planes[1].data().iter().all(|&v| v == 2));
    assert_eq!(f.residency(), FrameResidency::Dmabuf);
    // Exported as one plane per layout with its offset in the buffer, as another process
    // (or a GPU) imports it.
    let FrameBackingExport::DmabufPlanes { planes: fds } = f.export_backing().unwrap() else {
        panic!("not a dma-buf export");
    };
    assert_eq!(
        fds.iter().map(|p| (p.offset, p.len)).collect::<Vec<_>>(),
        [(0, w * h), (w * h, w * h / 2)]
    );
    let imported = FrameLease::from_dmabuf(f.meta().clone(), f.layouts(), fds).expect("import");
    assert!(imported.planes()[1].data().iter().all(|&v| v == 2));
    drop(imported);
    assert!(
        matches!(rx.recv(), RecvOutcome::Empty),
        "returned while held"
    );
    drop(planes);
    drop(f);
    assert!(matches!(rx.recv(), RecvOutcome::Data((0, 3))));
}

/// With early access (the default), the lease is handed over with CPU access already
/// started: the consumer's reads find the window open; it closes when the lease drops.
/// A prefetch reads the same pixels the lease shows.
#[test]
fn early_access_opens_the_window_on_the_capture_thread() {
    use crate::capture_api::NativeCpuPrefetch as P;
    let (w, h) = (64, 32);
    let spec = OutputSpec {
        code: FourCc::NV12,
        width: w as u32,
        height: h as u32,
    };
    let (tx, rx) = returns();
    let live = CaptureMetrics::default();
    let placed = Placed {
        spec,
        stride: w,
        size: None,
        crop: None,
    };
    for prefetch in [P::Off, P::Touch, P::L3] {
        let prep = CpuPrep {
            sync: true,
            prefetch,
        };
        let f = lease(
            placed,
            buffer(w, h),
            (0, 3),
            &frame(),
            &tx,
            &live,
            None,
            prep,
            None,
            Default::default(),
        );
        assert!(f.planes()[0].data().iter().all(|&v| v == 1), "{prefetch:?}");
        drop(f);
        assert!(matches!(rx.recv(), RecvOutcome::Data((0, 3))));
    }
    // Lazy: nothing until a read.
    let lazy = BeBacking {
        buffer: buffer(w, h),
        planes: SmallVec::from_slice(&[(0, w * h)]),
        output: 0,
        index: 1,
        window: CpuReadWindow::new(),
        returns: tx.clone(),
        progress: None,
    };
    assert!(!lazy.window.is_open());
    lazy.window.hold(|| lazy.sync(true));
    assert!(lazy.window.is_open());
    // A prefetch implies the sync: reading first would cache stale lines.
    let cfg = crate::capture_api::NativeIspConfig {
        early_cpu_sync: false,
        cpu_prefetch: P::L3,
        ..Default::default()
    };
    assert_eq!(
        CpuPrep::new(&cfg),
        CpuPrep {
            sync: true,
            prefetch: P::L3
        }
    );
    assert!(CpuPrep::new(&Default::default()).sync);
}

/// A sensor window (Styx's OV9782 1280x480: rows 160..639 of the 1280x800 field) places
/// a frame, and any back end crop of it, in the field's coordinates.
#[test]
fn crops_are_reported_in_the_sensors_field() {
    let w = styx_pisp::device::SensorWindow {
        x: 0,
        y: 160,
        width: 1280,
        height: 480,
        field: (1280, 800),
    };
    assert_eq!(in_field(w, None), FrameRect::new(0, 160, 1280, 480));
    let roi = FrameRect::new(100, 20, 256, 128);
    assert_eq!(in_field(w, Some(roi)), FrameRect::new(100, 180, 256, 128));
}

/// A crop sits in the top left of the full-size buffer: the planes keep the buffer's stride
/// and the chroma plane its offset.
#[test]
fn cropped_leases_cover_the_crop_in_the_full_size_buffer() {
    let (w, h) = (64, 32);
    let spec = OutputSpec {
        code: FourCc::NV12,
        width: w as u32,
        height: h as u32,
    };
    let (tx, _rx) = returns();
    let live = CaptureMetrics::default();
    let crop = FrameRect::new(8, 4, 32, 16);
    let placed = Placed {
        spec,
        stride: w,
        size: None,
        crop: Some(crop),
    };
    let f = lease(
        placed,
        buffer(w, h),
        (0, 3),
        &frame(),
        &tx,
        &live,
        None,
        CpuPrep::LAZY,
        None,
        Default::default(),
    );
    let res = f.meta().format.resolution;
    assert_eq!((res.width.get(), res.height.get()), (32, 16));
    assert_eq!(f.meta().crop, Some(crop));
    assert_eq!(f.plane_strides().as_slice(), &[w, w]);
    let planes = f.planes();
    assert_eq!(planes[0].data().len(), w * 16);
    assert!(planes[1].data().iter().all(|&v| v == 2));
    let FrameBackingExport::DmabufPlanes { planes: fds } = f.export_backing().unwrap() else {
        panic!("not a dma-buf export");
    };
    assert_eq!(
        fds.iter().map(|p| (p.offset, p.len)).collect::<Vec<_>>(),
        [(0, w * 16), (w * h, w * 8)]
    );
}
