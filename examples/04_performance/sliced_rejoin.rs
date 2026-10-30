//! Sliced frames through a camera service across idle pauses: a client joins, reads the first
//! frames, leaves, and the camera pauses for idle before the next join. Reports, per join, how
//! long the first frame took and whether each sliced frame completed or failed.
//!
//! ```text
//! sliced_rejoin serve PATH [SECONDS]     a camera service slicing at line 600, frames handed
//!                                        out at their first slice,
//!                                        pausing 1 s after its last reading client
//! sliced_rejoin client PATH [JOINS]      join JOINS times (default 5), 1.5 s apart
//! ```
//!
//! Styx's debug log
//! goes to stderr (why a sliced frame failed: the camera, or the service's wait for it).

use std::time::{Duration, Instant};

use styx::capture_api::StyxConfig;
use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;

const FRAMES_PER_JOIN: usize = 10;

fn serve(path: &str, seconds: u64) -> Result<(), Box<dyn std::error::Error>> {
    let camera = probe_all().into_iter().next().ok_or("no camera")?;
    let config = StyxConfig::new()
        .native_slice_line(Some(600))
        .native_deliver_slices(true);
    let _service = CameraService::new(camera)
        .capture_config(config)
        .pause_when_idle(Duration::from_secs(1))
        .serve(path)?;
    println!("camera service on {path} for {seconds} s");
    std::thread::sleep(Duration::from_secs(seconds));
    Ok(())
}

fn client(path: &str, joins: usize) -> Result<(), Box<dyn std::error::Error>> {
    let (mut failed, mut sliced) = (0, 0);
    for join in 0..joins {
        let t = Instant::now();
        let client = FrameClient::options(path)
            .sliced_frames()
            .request(&Frames::gray().size(1280, 800).pyramid(1))?;
        let mut first = None;
        let mut states = String::new();
        let mut n = 0;
        while n < FRAMES_PER_JOIN && t.elapsed() < Duration::from_secs(10) {
            let RecvOutcome::Data(frame) = client.recv(Duration::from_millis(500)) else {
                continue;
            };
            first.get_or_insert(t.elapsed());
            n += 1;
            let Some(handle) = frame.progress_handle() else {
                states.push('w');
                continue;
            };
            sliced += 1;
            let rows = handle.get().rows();
            let got = Instant::now();
            drop(frame);
            let end = handle.get().wait_complete(Duration::from_secs(2));
            let mark = match end {
                ProgressWait::Complete => 'c',
                ProgressWait::Failed => {
                    failed += 1;
                    println!(
                        "  join {join} frame {n}: failed {:.1} ms after receipt ({rows} rows then, {} now)",
                        got.elapsed().as_secs_f64() * 1e3,
                        handle.get().rows()
                    );
                    'F'
                }
                _ => 't',
            };
            states.push(mark);
        }
        println!(
            "join {join}: first frame after {:.1} ms; frames {states} (c complete, F failed, t timed out, w whole)",
            first.map_or(f64::NAN, |d| d.as_secs_f64() * 1e3)
        );
        drop(client);
        std::thread::sleep(Duration::from_millis(1500));
    }
    println!("{failed} of {sliced} sliced frames failed over {joins} joins");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    let num = |i: usize, d| arg(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    match arg(0) {
        Some("serve") => serve(arg(1).ok_or("socket path")?, num(2, 60) as u64),
        Some("client") => client(arg(1).ok_or("socket path")?, num(2, 5)),
        _ => Err("usage: sliced_rejoin serve PATH [SECONDS] | client PATH [JOINS]".into()),
    }
}
