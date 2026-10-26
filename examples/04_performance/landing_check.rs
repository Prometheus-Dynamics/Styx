//! On a device: a native camera's processed frame rate per mode, and the frame a manual
//! exposure lands on against the one the control predicted (`AppliedControl::frame`), at
//! 1280x800 at 30 and 120 fps and 640x400 at 240 fps.
//!
//! ```text
//! landing_check
//! ```

use std::time::Duration;

use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let camera = styx::probe_all()
        .into_iter()
        .find(|d| d.backends.iter().any(|b| b.kind == BackendKind::Native))
        .ok_or("no native camera")?;
    println!("camera: {}", camera.identity.display);
    for (w, h, fps, exposure_us) in [
        (1280, 800, 30, 8000),
        (1280, 800, 120, 4000),
        (640, 400, 240, 2000),
    ] {
        let plan = styx::planner::plan_frames(&camera, &Frames::nv12().size(w, h).fps(fps))?;
        let mut frames = plan.start()?;
        let mut stamps = Vec::new();
        while stamps.len() < 2 * fps as usize {
            if let RecvOutcome::Data(f) = frames.next_frame(Duration::from_secs(2)) {
                stamps.push(f.meta().timestamp);
            }
        }
        let span = (stamps[stamps.len() - 1] - stamps[0]) as f64 / 1e9;
        let measured = (stamps.len() - 1) as f64 / span;
        let applied = frames.set_exposure_us(exposure_us)?;
        let mut landed = None;
        for _ in 0..fps * 2 {
            let RecvOutcome::Data(f) = frames.next_frame(Duration::from_secs(2)) else {
                continue;
            };
            let Some(n) = f.meta().native().copied() else {
                continue;
            };
            let got = n.exposure_ns as f64 / 1e3;
            if (got - f64::from(exposure_us)).abs() <= f64::from(exposure_us) * 0.05 {
                landed = Some(n.sequence);
                break;
            }
        }
        println!(
            "{w}x{h}@{fps}: {measured:.2} fps; exposure {exposure_us} us predicted frame {:?}, landed {:?}, clamped {}",
            applied.frame, landed, applied.clamped
        );
        drop(frames);
    }
    Ok(())
}
