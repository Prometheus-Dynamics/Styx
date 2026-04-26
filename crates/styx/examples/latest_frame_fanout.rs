use std::num::NonZeroU32;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use styx::DeviceIdentity;
use styx::core::queue::NewestRx;
use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Fanout branches that only care about the freshest frame should not
    // backpressure capture. A shallow capture queue plus `newest()` branches
    // keeps slow consumers from building latency.
    StyxConfig::new()
        .capture_queue_depth(2)
        .capture_pool(4, 1 << 18, 4)
        .apply();

    let device = virtual_device();
    let mode = device.backends[0].descriptor.modes[0].clone();
    let handle = CaptureRequest::new(&device).mode(mode.id.clone()).start()?;

    let (preview_tx, preview_rx) = newest::<Arc<FrameLease>>();
    let (analysis_tx, analysis_rx) = newest::<Arc<FrameLease>>();

    let producer = thread::spawn(move || {
        let started = Instant::now();
        let mut pushed = 0u32;
        while started.elapsed() < Duration::from_secs(2) {
            match handle.recv_blocking(Duration::from_millis(5)) {
                RecvOutcome::Data(frame) => {
                    pushed = pushed.saturating_add(1);
                    let shared = Arc::new(frame);
                    let _ = preview_tx.send(Arc::clone(&shared));
                    let _ = analysis_tx.send(shared);
                }
                RecvOutcome::Empty => {}
                RecvOutcome::Closed => break,
            }
        }
        handle.stop();
        preview_tx.close();
        analysis_tx.close();
        pushed
    });

    let preview_worker = thread::spawn(move || consume_latest("preview", preview_rx, 12));
    let analysis_worker = thread::spawn(move || consume_latest("analysis", analysis_rx, 40));

    let pushed = producer.join().expect("producer");
    let preview_seen = preview_worker.join().expect("preview worker");
    let analysis_seen = analysis_worker.join().expect("analysis worker");

    println!("fanout pushed={pushed} preview_seen={preview_seen} analysis_seen={analysis_seen}");
    Ok(())
}

fn consume_latest(name: &str, rx: NewestRx<Arc<FrameLease>>, sleep_ms: u64) -> u32 {
    let started = Instant::now();
    let mut seen = 0u32;
    let mut last_timestamp = None;

    while started.elapsed() < Duration::from_secs(2) {
        match rx.recv() {
            RecvOutcome::Data(frame) => {
                let timestamp = frame.meta().timestamp;
                if last_timestamp != Some(timestamp) {
                    seen = seen.saturating_add(1);
                    last_timestamp = Some(timestamp);
                    println!(
                        "{name} latest ts={} stride={}",
                        timestamp,
                        frame.plane_strides().first().copied().unwrap_or_default()
                    );
                }
            }
            RecvOutcome::Empty => {}
            RecvOutcome::Closed => break,
        }
        thread::sleep(Duration::from_millis(sleep_ms));
    }

    seen
}

fn virtual_device() -> ProbedDevice {
    let res = Resolution::new(640, 360).unwrap();
    let interval = Interval {
        numerator: NonZeroU32::new(1).unwrap(),
        denominator: NonZeroU32::new(30).unwrap(),
    };
    let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
    let mode = Mode {
        id: ModeId {
            format,
            interval: Some(interval),
        },
        format,
        intervals: vec![interval].into(),
        interval_stepwise: None,
    };
    let descriptor = CaptureDescriptor {
        modes: vec![mode.clone()],
        controls: Vec::new(),
    };
    let backend = ProbedBackend {
        kind: BackendKind::Virtual,
        handle: BackendHandle::Virtual,
        descriptor: descriptor.clone(),
        properties: vec![("kind".into(), "virtual".into())],
    };
    ProbedDevice {
        identity: DeviceIdentity {
            display: "virtual-latest-fanout".into(),
            keys: vec!["virtual".into()],
        },
        backends: vec![backend],
    }
}
