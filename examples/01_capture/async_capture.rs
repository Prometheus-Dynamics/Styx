//! Async capture, with Tokio and without any runtime.
//!
//! Frames are awaited (`PlannedFrames::next_frame_async`, `CaptureHandle::recv_async`); a
//! frame wakes the task that waits for it, nothing polls. The futures need no particular
//! runtime: the same `consume` function runs under Tokio next to a timer, and on a ten-line
//! `block_on` made of `std` alone (`--no-runtime`).
//!
//! ```sh
//! cargo run -p styx-examples --features async,native,v4l2 --bin async_capture            # Tokio
//! cargo run -p styx-examples --features async,native,v4l2 --bin async_capture -- --no-runtime
//! ```
//!
//! Starting a capture is synchronous (it opens devices, tens of milliseconds): under Tokio it
//! goes to `spawn_blocking` so the runtime's threads keep running other tasks meanwhile.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake};
use std::time::{Duration, Instant};

use styx::planner::PlannedFrames;
use styx::prelude::*;

/// What this example asks for: NV12 at 640x400 or the closest size above, 30 fps.
fn start() -> Result<PlannedFrames, Box<dyn std::error::Error + Send + Sync>> {
    let wants = FrameRequirements::formats([FourCc::NV12])
        .output_resolution(640, 400)
        .min_fps(30)
        .priority(Priority::Power);
    let plan = styx::planner::plan_best(&styx::probe_all(), &wants)?;
    print!("{plan}");
    Ok(plan.start()?)
}

/// Awaits `count` frames and reports how long each wait took. Runtime-agnostic.
async fn consume(frames: &mut PlannedFrames, count: usize) -> usize {
    let mut got = 0;
    let mut waited = Duration::ZERO;
    while got < count {
        let t = Instant::now();
        match frames.next_frame_async().await {
            RecvOutcome::Data(frame) => {
                waited += t.elapsed();
                got += 1;
                if got % 30 == 0 {
                    let res = frame.meta().format.resolution;
                    println!(
                        "  frame {} {}x{}, mean wait {:.1} ms",
                        frame.meta().sequence().unwrap_or(0),
                        res.width,
                        res.height,
                        waited.as_secs_f64() * 1e3 / got as f64
                    );
                }
            }
            RecvOutcome::Empty => {}
            RecvOutcome::Closed => break,
        }
    }
    got
}

/// Runs a future to completion on this thread: park until woken, poll again.
fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Arc::new(Unpark(std::thread::current())).into();
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        std::thread::park();
    }
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if std::env::args().any(|a| a == "--no-runtime") {
        println!("no runtime: std block_on");
        let mut frames = start()?;
        let got = block_on(consume(&mut frames, 90));
        println!("{got} frames");
        frames.stop();
        return Ok(());
    }
    tokio_main()
}

#[tokio::main(flavor = "current_thread")]
async fn tokio_main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Tokio, current-thread runtime");
    let mut frames = tokio::task::spawn_blocking(start).await??;
    // The runtime stays free while frames are awaited: a timer ticks on the same thread.
    let mut ticks = 0u32;
    let mut timer = tokio::time::interval(Duration::from_millis(500));
    let got = {
        let mut consumer = pin!(consume(&mut frames, 90));
        loop {
            tokio::select! {
                got = &mut consumer => break got,
                _ = timer.tick() => ticks += 1,
            }
        }
    };
    println!("{got} frames; the timer ticked {ticks} times meanwhile");
    // Stopping joins the capture threads: off the runtime's thread too.
    tokio::task::spawn_blocking(move || frames.stop()).await?;
    Ok(())
}
