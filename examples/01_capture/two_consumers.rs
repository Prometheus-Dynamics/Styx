//! Two consumers of one camera at different sizes and formats: one capture, planned for both
//! (`plan_many`). On the CM5 the PiSP back end makes both in one pass (output 0: NV12 at the
//! sensor size for a recorder, output 1: RGB at half size for a detector), nothing converted on
//! the CPU; elsewhere the planner shares the capture and converts or scales per consumer. Each
//! consumer runs on its own thread at its own pace: a slow one drops its own frames, never
//! the other's.
//!
//! ```sh
//! cargo run -p styx-examples --features native,v4l2 --bin two_consumers -- [seconds]
//! ```

use std::thread;
use std::time::{Duration, Instant};

use styx::planner::plan_many;
use styx::prelude::*;

fn consume(name: &'static str, mut frames: Frames, run: Duration, work: Duration) {
    let started = Instant::now();
    let (mut n, mut size, mut code, mut transport) = (0u32, (0, 0), String::new(), "");
    while started.elapsed() < run {
        let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_millis(500)) else {
            continue;
        };
        n += 1;
        let meta = frame.meta();
        size = (
            meta.format.resolution.width.get(),
            meta.format.resolution.height.get(),
        );
        code = meta.format.code.to_string();
        transport = match meta.residency {
            Some(FrameResidency::Dmabuf) => "dma-buf",
            _ => "memory",
        };
        thread::sleep(work); // the consumer's own processing
    }
    println!(
        "{name}: {n} frames {code} {}x{} ({transport}), {:.1} fps, {} dropped",
        size.0,
        size.1,
        f64::from(n) / run.as_secs_f64(),
        frames.dropped(),
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let devices = styx::probe_all();
    // Prefer a camera with an ISP (native), else the first one.
    let device = devices
        .iter()
        .find(|d| d.backend(BackendKind::Native).is_some())
        .or(devices.first())
        .ok_or("no camera")?;
    // Each consumer gets the latest frame only (the default delivery). Frames waiting in a queue
    // hold camera buffers: on the PiSP each output has 4 (`StyxConfig::native_output_buffers`),
    // and a slow consumer with a deeper queue (`every_frame(n)`) would hold them all and slow
    // the camera for everyone.
    let recorder = Frames::nv12().fps(30);
    let detector = Frames::rgb().size(640, 400);
    let plan = plan_many(device, &[recorder, detector])?;
    print!("{plan}");

    let mut consumers = plan.start()?.into_iter();
    let (recorder, detector) = (consumers.next().ok_or("")?, consumers.next().ok_or("")?);
    let run = Duration::from_secs(seconds);
    let a = thread::spawn(move || consume("recorder", recorder, run, Duration::ZERO));
    // The detector takes 50 ms per frame: it gets about 20 frames a second, the recorder all.
    let b = thread::spawn(move || consume("detector", detector, run, Duration::from_millis(50)));
    a.join().ok();
    b.join().ok();
    Ok(())
}
