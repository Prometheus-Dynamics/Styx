//! Quickstart: open the first camera and read ten frames from it.
//!
//! Run it on the simulated camera, which needs no hardware:
//!
//! ```text
//! cargo run -p styx-examples --example quickstart -- --sim
//! ```
//!
//! Without `--sim` it opens the first camera the chooser plans. A native camera needs the Lemnos
//! daemon's socket (`--lemnos SOCKET`); libcamera and V4L2 cameras need nothing else.
//!
//! A frame is a `FrameLease`: the camera's buffer, held until the lease is dropped. Its id counts
//! up for the stream (the first read may start past 0, since the camera runs before the first read),
//! its timestamp is `CLOCK_MONOTONIC` in nanoseconds when the frame was made, and its format says
//! what the bytes are. The metadata says which controls were in force.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use styx_examples::{Lab, flag, value, with_first_camera};

/// How many frames to read.
const FRAMES: usize = 10;
/// How long to wait for each frame before giving up.
const WAIT: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("quickstart: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let lab = if flag("--sim") {
        Lab::sim("quickstart")?
    } else {
        Lab::real(value("--lemnos").map(PathBuf::from))
    };
    // `with_first_camera` starts the owner's capture and calls this closure with it. The capture
    // stops when the closure returns.
    with_first_camera(&lab, |camera, selected| -> Result<(), String> {
        println!(
            "camera {} owned by {} ({})",
            selected.id, selected.owner, selected.reason
        );
        let mut first: Option<(u64, u64)> = None;
        let mut last = (0_u64, 0_u64);
        for _ in 0..FRAMES {
            let frame = match camera.next_frame(WAIT).map_err(|e| e.to_string())? {
                Some(frame) => frame,
                None => return Err(format!("no frame within {} s", WAIT.as_secs())),
            };
            let format = frame.format();
            println!(
                "frame id={} timestamp_ns={} format={} {}x{} exposure_us={} analogue_gain_milli={}",
                frame.id(),
                frame.timestamp_ns(),
                format.code,
                format.resolution.width,
                format.resolution.height,
                frame.meta().exposure_us,
                frame.meta().analogue_gain_milli,
            );
            let stamp = (frame.id(), frame.timestamp_ns());
            if first.is_none() {
                first = Some(stamp);
            }
            last = stamp;
            // The frame is given back to the camera when it drops here.
        }
        // Frames per second over the span of the ten: nine intervals between the first and last.
        if let Some((_, t0)) = first {
            let span_s = (last.1 - t0) as f64 / 1e9;
            if span_s > 0.0 {
                println!(
                    "achieved {:.1} frames/s over {} frames ({:.4} s)",
                    (FRAMES - 1) as f64 / span_s,
                    FRAMES,
                    span_s
                );
            }
        }
        Ok(())
    })?
}
