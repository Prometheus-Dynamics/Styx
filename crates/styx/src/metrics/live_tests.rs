use std::sync::Arc;
use std::time::Duration;

use styx_core::prelude::*;

use super::*;

fn meta(sequence: u32, timestamp_ms: u64, error: bool) -> FrameMeta {
    let res = Resolution::new(4, 4).unwrap();
    let mut meta = FrameMeta::new(
        MediaFormat::new(FourCc::NV12, res, ColorSpace::Srgb),
        timestamp_ms * 1_000_000,
    )
    .with_backend(BackendFrameMeta::Native(NativeFrameMeta {
        sequence,
        error,
        exposure_ns: 10_000_000,
        analog_gain: 2.0,
        digital_gain: 1.0,
        ..Default::default()
    }));
    meta.clock = Some(TimestampClock::Monotonic);
    meta
}

#[test]
fn drops_are_told_apart_by_cause() {
    let m = CaptureMetrics::default();
    // 30 fps: frames 0, 1, then 3 (one lost by the sensor), the ISP skips one (4), then 5.
    m.frame(&meta(0, 1000, false));
    m.frame(&meta(1, 1033, false));
    m.frame(&meta(3, 1100, false));
    m.isp_skipped();
    m.frame(&meta(5, 1166, true));
    let s = m.snapshot();
    assert_eq!(s.frames.captured, 4);
    assert_eq!(s.drops.sensor_sequence_gaps, 1);
    assert_eq!(s.drops.isp_skipped, 1);
    assert_eq!(s.drops.corrupted, 1);
    assert_eq!(s.drops.total, 3);
    // Mean interval 55.3 ms over the three gaps.
    let fps = s.fps.measured.unwrap();
    assert!((fps - 3.0 / 0.166).abs() < 0.1, "{fps}");
    let aaa = s.aaa.unwrap();
    assert_eq!(aaa.exposure_us, Some(10_000.0));
    assert_eq!(aaa.analogue_gain, Some(2.0));
    assert_eq!(aaa.ae_state, None);
    // The timestamps are long past: every frame has a delivery latency.
    assert_eq!(s.latency.sensor_to_delivery.samples, 4);
}

#[test]
fn uvc_gaps_count_as_corrupt_frames() {
    let m = CaptureMetrics::default().gaps_are_corrupt();
    let uvc = |sequence| {
        let mut meta = meta(0, 1000 + u64::from(sequence) * 33, false);
        meta.backend = Some(BackendFrameMeta::Uvc(UvcFrameMeta {
            sequence,
            ..Default::default()
        }));
        meta
    };
    m.frame(&uvc(0));
    m.frame(&uvc(3));
    let s = m.snapshot();
    assert_eq!(s.drops.corrupted, 2);
    assert_eq!(s.drops.sensor_sequence_gaps, 0);
}

#[test]
fn windows_report_percentiles_and_maxima() {
    let ring = Ring::default();
    for ms in 1..=200u64 {
        ring.push(ms * 1_000_000);
    }
    let w = ring.window();
    assert_eq!((w.samples, w.total), (WINDOW as u64, 200));
    // The window holds the last 128 samples: 73..=200 ms.
    assert_eq!(w.max_ms, Some(200.0));
    assert_eq!(w.max_ever_ms, Some(200.0));
    let p50 = w.p50_ms.unwrap();
    assert!((136.0..=137.0).contains(&p50), "{p50}");
    assert!(Window::from_samples(Vec::new(), 0, 0).p50_ms.is_none());
}

struct Heap(Vec<u8>);

impl ExternalBacking for Heap {
    fn plane_data(&self, _: usize) -> Option<&[u8]> {
        Some(&self.0)
    }
    fn backing_bytes(&self) -> Option<usize> {
        Some(self.0.len())
    }
}

#[test]
fn buffers_are_counted_while_frames_hold_them() {
    let m = CaptureMetrics::default();
    let res = Resolution::new(4, 4).unwrap();
    let format = MediaFormat::new(FourCc::new(*b"GREY"), res, ColorSpace::Unknown);
    let frame = FrameLease::from_external(
        FrameMeta::new(format, 0),
        smallvec::smallvec![plane_layout_from_dims(res.width, res.height, 1)],
        Arc::new(m.track(Heap(vec![7; 16]))),
    );
    let share = frame.into_shareable();
    let other = share.share().unwrap();
    assert_eq!(other.planes()[0].data()[0], 7);
    let s = m.snapshot();
    assert_eq!((s.buffers.held, s.buffers.held_bytes), (1, 16));
    drop(share);
    std::thread::sleep(Duration::from_millis(2));
    drop(other);
    let s = m.snapshot();
    assert_eq!((s.buffers.held, s.buffers.peak_held), (0, 1));
    assert_eq!(s.buffers.hold.samples, 1);
    assert!(s.buffers.hold.max_ms.unwrap() >= 2.0);
}

#[test]
fn a_reconnecting_capture_adds_up_its_backend_captures() {
    let outer = CaptureMetrics::default();
    let first = CaptureMetrics::default();
    outer.set_current(Some(first.clone()));
    first.frame(&meta(0, 1000, false));
    first.frame(&meta(2, 1066, false));
    outer.absorb(&first);
    let second = CaptureMetrics::default();
    outer.set_current(Some(second.clone()));
    second.frame(&meta(0, 2000, false));
    let s = outer.snapshot();
    assert_eq!(s.frames.captured, 3);
    assert_eq!(s.drops.sensor_sequence_gaps, 1);
}

#[test]
fn consumers_count_what_they_get_and_hold() {
    let m = CaptureMetrics::default();
    let c = ConsumerStats::new("client 1");
    m.add_consumer(&c);
    c.sent();
    c.sent();
    c.released(Duration::from_millis(5));
    c.dropped();
    let s = m.snapshot();
    assert_eq!(s.consumers.len(), 1);
    let c = &s.consumers[0];
    assert_eq!((c.received, c.dropped, c.held), (2, 1, 1));
    assert_eq!(c.hold.p50_ms, Some(5.0));
}

#[test]
fn the_text_exposition_names_every_camera() {
    let m = CaptureMetrics::default();
    m.frame(&meta(0, 1000, false));
    let snapshot = super::super::MetricsSnapshot {
        cameras: vec![m.snapshot()],
        ..Default::default()
    };
    let text = snapshot.prometheus_text();
    assert!(text.contains("# TYPE styx_camera_frames_total counter"));
    assert!(text.contains("styx_camera_frames_total{camera=\"\",id=\""));
    assert!(text.contains("cause=\"sensor_sequence_gap\"} 0"));
    assert!(text.contains("styx_camera_exposure_us{"));
    // HELP and TYPE once per metric.
    assert_eq!(text.matches("# TYPE styx_camera_drops_total").count(), 1);
}

#[test]
fn autofocus_state_lens_and_scans_are_reported() {
    use super::super::af::{AfModeKind, AfSample, AfStateKind};
    let m = CaptureMetrics::default();
    let sample = |state, lens, settled| AaaSample {
        af: Some(AfSample {
            state,
            mode: AfModeKind::Continuous,
            lens_dioptres: lens,
            lens_settled: settled,
        }),
        ..Default::default()
    };
    // Without a lens nothing AF is reported.
    m.aaa(&AaaSample::default());
    assert_eq!(m.snapshot().aaa.unwrap().af_state, None);
    // Two scans: idle, scanning (x2), focused, scanning again, failed.
    for (state, lens, settled) in [
        (AfStateKind::Idle, Some(1.0), Some(true)),
        (AfStateKind::Scanning, Some(2.0), Some(false)),
        (AfStateKind::Scanning, Some(3.0), Some(false)),
        (AfStateKind::Focused, Some(2.5), Some(true)),
        (AfStateKind::Scanning, Some(4.0), None),
        (AfStateKind::Failed, Some(4.5), Some(true)),
    ] {
        m.aaa(&sample(state, lens, settled));
    }
    let a = m.snapshot().aaa.unwrap();
    assert_eq!(a.af_state.as_deref(), Some("failed"));
    assert_eq!(a.af_mode.as_deref(), Some("continuous"));
    assert_eq!(a.lens_position_dioptres, Some(4.5));
    assert_eq!(a.lens_settled, Some(true));
    assert_eq!(a.af_scans, Some(2));
    m.aaa(&sample(AfStateKind::Scanning, None, None));
    let a = m.snapshot().aaa.unwrap();
    assert_eq!((a.lens_position_dioptres, a.lens_settled), (None, None));
    assert_eq!(a.af_scans, Some(3));
    let text = super::super::MetricsSnapshot {
        cameras: vec![m.snapshot()],
        ..Default::default()
    }
    .prometheus_text();
    assert!(
        text.contains("state=\"scanning\",mode=\"continuous\"} 1"),
        "{text}"
    );
    assert!(text.contains("styx_camera_af_scans_total{"), "{text}");
}

#[test]
fn taken_frames_record_their_hops_and_copies() {
    let m = CaptureMetrics::default();
    let now = TimestampClock::Monotonic.now_ns().unwrap();
    for i in 0..10u32 {
        let mut f = meta(i, 0, false);
        f.timestamp = now - 9_000_000;
        f.hops.set(Hop::Dequeued, now - 3_000_000);
        f.hops.set(Hop::IspDone, now - 1_000_000);
        if i == 9 {
            styx_core::metrics::copied_frame(&mut f, styx_core::metrics::CopySite::Region, 100);
        }
        stamp_queued(&mut f);
        assert_eq!(f.hops.get(Hop::Sensor), Some(now - 9_000_000));
        assert_eq!(f.hops.sequence(), Some(i));
        m.frame(&f);
        m.taken(&mut f);
        assert!(f.hops.get(Hop::Taken) >= f.hops.get(Hop::Queued));
    }
    let s = m.snapshot();
    let p = &s.path;
    assert_eq!(
        (p.frames, p.zero_copy, p.copied, p.copied_bytes),
        (10, 9, 1, 100)
    );
    let names: Vec<_> = p
        .hops
        .iter()
        .map(|h| (h.from.as_str(), h.to.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("sensor", "dequeued"),
            ("dequeued", "isp_done"),
            ("isp_done", "queued"),
            ("queued", "taken")
        ]
    );
    let dequeue = p.hop("dequeued").unwrap().window.p50_ms.unwrap();
    assert!((dequeue - 6.0).abs() < 1e-6, "{dequeue}");
    assert!(p.total.p99_ms.unwrap() >= 9.0);
    let snapshot = super::super::MetricsSnapshot {
        cameras: vec![s],
        process: super::super::process(),
        ..Default::default()
    };
    let text = snapshot.prometheus_text();
    for name in [
        "styx_camera_hop_ms{",
        "from=\"isp_done\",to=\"queued\",quantile=\"0.99\"",
        "styx_camera_path_ms{",
        "styx_camera_path_frames_total{",
        "styx_process_copies_total{site=\"region\"}",
        "styx_process_dmabuf_syncs_total",
        "styx_process_pool_exhausted_total",
    ] {
        assert!(text.contains(name), "{name} missing:\n{text}");
    }
}
