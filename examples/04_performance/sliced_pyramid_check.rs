//! On a device: a native camera service with sliced delivery (`native_slice_line`,
//! `native_deliver_slices`) and a client asking for NV12 with a three-level pyramid (extra
//! back end passes), holding each frame for a while as a vision graph does. The capture must
//! keep the sensor's rate: no frame dropped for want of a back end buffer.
//!
//! ```text
//! sliced_pyramid_check [HOLD_MS] [SECONDS] [whole]    defaults: 51 ms, 10 s, sliced
//! ```

use std::time::{Duration, Instant};
use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let hold = Duration::from_millis(args.first().and_then(|a| a.parse().ok()).unwrap_or(51));
    let secs: f64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(10.0);
    let sliced = args.iter().all(|a| a != "whole");
    let path = std::env::temp_dir().join(format!("styx-sliced-pyr-{}.sock", std::process::id()));
    let camera = probe_all().into_iter().next().ok_or("no camera")?;
    let config = StyxConfig::default()
        .native_slice_line(sliced.then_some(600))
        .native_deliver_slices(sliced);
    let service = CameraService::new(camera)
        .capture_config(config)
        .keep_streaming()
        .serve(&path)?;
    let client =
        FrameClient::options(&path).request(&Frames::nv12().size(1280, 800).fps(30).pyramid(3))?;
    let (start, mut n, mut companions) = (Instant::now(), 0u32, 0usize);
    while start.elapsed().as_secs_f64() < secs {
        if let RecvOutcome::Data(f) = client.recv(Duration::from_millis(200)) {
            n += 1;
            companions = f.companions().count();
            std::thread::sleep(hold);
        }
    }
    let fps = f64::from(n) / start.elapsed().as_secs_f64();
    let m = service.metrics();
    let drops = m.snapshot.cameras.first().map(|c| c.drops.clone());
    println!(
        "sliced {sliced}, hold {} ms: {n} frames, {fps:.1} fps, {companions} companions; drops {drops:?}",
        hold.as_millis()
    );
    Ok(())
}
