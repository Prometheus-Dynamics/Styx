//! Frames and controls of a bridged camera from async code (tokio here; any executor works)
//! and from plain threads, with the cancellation rules.
//!
//! ```sh
//! STYX_SENSOR_PATH=/tmp/styx/ov9782.toml async_frames [seconds]
//! ```
//!
//! - [`FrameStream`] is a `futures_core::Stream`. Waiting needs no runtime of its own: the
//!   camera's event thread polls the capture node and wakes the task's waker, so it works
//!   under tokio, smol, `styx_graph::rt::block_on`, or [`FrameStream::next_blocking`].
//! - Cancelling a wait (a `select!` branch losing, a `timeout`, an aborted task) loses no
//!   frame: a frame leaves the capture queue only in the poll that returns it.
//! - Dropping a frame gives its buffer back; frames may outlive the stream and the camera
//!   (they keep their memory, and never go back to a newer stream's queue).
//! - Controls ([`styx_native::CameraControls`]) are cheap, cloneable, `Send` handles; calls
//!   are short (register writes are scheduled for frame starts), fine from async code.
//! - Stopping or dropping the camera ends the stream (`None`); a fault ends it after one
//!   error item.

use std::time::Duration;

use styx_native::{FrameStream, NativeCamera, NativeError, SensorLibrary, StreamSettings};

fn main() -> Result<(), NativeError> {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(5);
    let (mut cameras, problems) = styx_native::discover(&SensorLibrary::system());
    for p in problems {
        eprintln!("problem: {p}");
    }
    let Some(info) = cameras.pop() else {
        eprintln!("no bridged camera");
        return Ok(());
    };
    let mut camera = NativeCamera::open(info, Default::default())?;
    camera.configure(&StreamSettings::new(1280, 800).fps(60))?;

    // 1. Under tokio, with a control task and a ticker competing in `select!`.
    let stream = camera.start()?;
    let controls = camera.controls();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
        .expect("tokio runtime");
    let (stream, frames) = runtime.block_on(async move {
        let ctl = tokio::spawn(async move {
            for fps in [30.0, 60.0, 120.0] {
                let landed = controls.set_frame_rate(fps).expect("frame rate");
                println!("{fps} fps lands on frame {:?}", landed.first().map(|l| l.frame));
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let mut stream = stream;
        let mut frames = 0u64;
        let mut ticks = tokio::time::interval(Duration::from_millis(250));
        let end = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            tokio::select! {
                // Losing this branch cancels the wait; no frame is lost.
                f = styx_graph::rt::next(&mut stream) => match f {
                    Some(Ok(f)) => {
                        frames += 1;
                        if frames.is_multiple_of(60) {
                            println!("frame {} {:?}", f.sequence, f.controls.map(|c| c.frame_duration));
                        }
                    }
                    Some(Err(e)) => { eprintln!("stream: {e}"); break; }
                    None => break,
                },
                _ = ticks.tick() => {
                    if tokio::time::Instant::now() >= end { break; }
                }
            }
        }
        let _ = ctl.await;
        (stream, frames)
    });
    println!("tokio: {frames} frames, {:?}", stream.stats());
    camera.stop()?;
    drop(stream);

    // 2. Without any runtime: the blocking wrapper on a plain thread.
    let mut stream = camera.start()?;
    let worker = std::thread::spawn(move || blocking_frames(&mut stream, 120));
    let n = worker.join().expect("worker")?;
    println!("blocking: {n} frames");
    camera.close()
}

fn blocking_frames(stream: &mut FrameStream, n: usize) -> Result<usize, NativeError> {
    let mut got = 0;
    while got < n {
        match stream.next_blocking(Duration::from_millis(500)) {
            Ok(Some(_)) => got += 1,
            Ok(None) => break,
            // A timeout cancels nothing either: just wait again.
            Err(NativeError::Timeout) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}
