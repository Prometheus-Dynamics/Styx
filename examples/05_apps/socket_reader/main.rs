//! Socket reader: read a camera's frames in another process, with no copy.
//!
//! The camera service publishes each camera's frames on a Unix socket of its own (`camera_socket`
//! names it). A reader connects, is given the service's buffers once, and then gets one ring entry
//! per frame: the frame's buffer is read where the service wrote it, and the frame's metadata
//! travels with it as opaque bytes.
//!
//! Run it on the simulated camera. It starts the camera service itself:
//!
//! ```text
//! cargo run -p styx-examples --example socket_reader -- --sim
//! ```
//!
//! Or read a service that is already running (`styx-service CONFIG`) by its camera's socket:
//!
//! ```text
//! cargo run -p styx-examples --example socket_reader -- --socket /run/styx/<camera socket>
//! ```
//!
//! With real cameras and no `--socket`, name the camera with `--camera ID` (for example
//! `lemnos:sensor0`) and, for a V4L2 or libcamera camera, its pixel format with `--format YUYV`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use styx_controls::FrameMetadata;
use styx_examples::{Lab, SIM_CAMERA, connect_reader, flag, serve, value};

/// How many frames to read.
const FRAMES: usize = 10;
/// How long to wait for the service to publish the camera.
const READY: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("socket_reader: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    if let Some(path) = value("--socket") {
        return read(Path::new(&path));
    }

    // No socket named: start a service on the camera, and read its socket.
    let sim = flag("--sim");
    let lab = if sim {
        Lab::sim("socket_reader")?
    } else {
        Lab::real(value("--lemnos").map(PathBuf::from))
    };
    let camera = if sim {
        SIM_CAMERA.to_string()
    } else {
        value("--camera").ok_or("name the camera with --camera ID, or give --socket PATH")?
    };
    let served = serve(&lab, "socket_reader", &camera, value("--format").as_deref())?;
    println!(
        "service on {} serves {camera}",
        served.service().control_socket().display()
    );
    read(&served.socket())
}

/// Connects to the camera socket at `path` and reads ten frames, printing each one.
fn read(path: &Path) -> Result<(), String> {
    let mut reader = connect_reader(path)?;
    println!("connected to {}", path.display());

    let mut previous: Option<u64> = None;
    let deadline = Instant::now() + READY;
    let mut count = 0;
    while count < FRAMES {
        let frame = match reader.next_frame().map_err(|e| e.to_string())? {
            Some(frame) => frame,
            None => {
                if Instant::now() > deadline {
                    return Err("no frame arrived".to_string());
                }
                reader
                    .wait(Some(Duration::from_millis(500)))
                    .map_err(|e| e.to_string())?;
                continue;
            }
        };
        let id = frame.lease().id();
        // Frames the producer published that this reader never got, between two that it did.
        let skipped = previous.map_or(0, |p| id.saturating_sub(p + 1));
        previous = Some(id);
        let meta = FrameMetadata::from_bytes(frame.user());
        let (exposure, gain) = meta.map_or((0, 0), |m| (m.exposure_us, m.analogue_gain_milli));
        println!(
            "frame id={id} timestamp_ns={} age_ns={} buffer={} skipped_before={skipped} exposure_us={exposure} analogue_gain_milli={gain} metadata={}",
            frame.lease().timestamp_ns(),
            frame.age_ns(),
            frame.buffer(),
            if meta.is_some() { "ok" } else { "missing" },
        );
        count += 1;
        // The frame's buffer goes back to the producer when `frame` drops here.
    }

    let stats = reader.stats();
    let copies: usize = styx_frame::copy_counters().iter().map(|c| c.copies()).sum();
    println!(
        "reader: delivered={} skipped={} copies={copies}",
        stats.delivered, stats.skipped
    );
    Ok(())
}
