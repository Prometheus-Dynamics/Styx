//! On a device: a camera service's capture after a client leaves. A 640x400 client alone, a
//! 1280x800 client joining (the camera moves to its mode), then leaving: how long until the
//! first client's own mode is back, and its frames keep coming throughout.
//!
//! ```text
//! service_leave_check
//! ```

use std::time::{Duration, Instant};

use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let camera = styx::probe_all()
        .into_iter()
        .find(|d| d.backends.iter().any(|b| b.kind == BackendKind::Native))
        .ok_or("no native camera")?;
    println!("camera: {}", camera.identity.display);
    let path = std::env::temp_dir().join(format!("styx-leave-{}.sock", std::process::id()));
    let service = CameraService::new(camera).keep_streaming().serve(&path)?;
    let mode = || {
        service
            .plan()
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    };
    let frames = |client: &FrameClient, n: usize| {
        (0..n * 3)
            .filter_map(|_| match client.recv(Duration::from_secs(2)) {
                RecvOutcome::Data(f) => Some(f.meta().format.resolution.width.get()),
                _ => None,
            })
            .take(n)
            .count()
    };
    let small = FrameClient::request(&path, &Frames::gray().size(640, 400).fps(30))?;
    frames(&small, 10);
    let alone = mode();
    println!("alone: {alone}");
    for round in 1..=3 {
        let big = FrameClient::request(&path, &Frames::gray().size(1280, 800).fps(30))?;
        frames(&big, 10);
        println!("round {round} with the second client: {}", mode());
        let left = Instant::now();
        drop(big);
        while mode() != alone && left.elapsed() < Duration::from_secs(30) {
            let _ = small.recv(Duration::from_millis(20));
        }
        let back = left.elapsed();
        let got = frames(&small, 30);
        println!(
            "round {round}: mode back after {back:.2?} ({}), then {got}/30 frames, restarts {}",
            if mode() == alone { "yes" } else { "NO" },
            service.stats().restarts
        );
    }
    service.stop();
    Ok(())
}
