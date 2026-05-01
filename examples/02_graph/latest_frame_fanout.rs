use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use styx::core::queue::NewestRx;
use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Fanout branches that only care about the freshest frame should not
    // backpressure capture. A shallow capture queue plus `newest()` branches
    // keeps slow consumers from building latency.
    let config = StyxConfig::new()
        .capture_queue_depth(2)
        .capture_pool(4, 1 << 18, 4);

    let device = virtual_device();
    let handle = device.capture_request().config(config).start()?;

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
    make_virtual_rgb_device("virtual-latest-fanout", 640, 360, 30)
}
