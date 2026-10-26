//! On a device: a vision consumer and a raw consumer of one native camera through a camera
//! service, and the AE controls a client sets live.
//!
//! ```text
//! raw_controls_check serve PATH [SECONDS]   a camera service on PATH
//! raw_controls_check client PATH            the checks, printed
//! ```
//!
//! The client reads luma + pyramid frames and the raw frames (`Frames::sensor_raw()`) of the
//! same capture for 5 s (rates, raw read speed, per-frame metadata), lists the exposure, gain
//! and menu controls (effective ranges, labels), caps AE's exposure and checks the frames keep
//! to it, then clears the cap.

use std::time::{Duration, Instant};

use styx::capture_api::native_controls as ctl;
use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;

fn serve(path: &str, seconds: u64) -> Result<(), Box<dyn std::error::Error>> {
    let camera = probe_all().into_iter().next().ok_or("no camera")?;
    let _service = CameraService::new(camera).keep_streaming().serve(path)?;
    println!("camera service on {path} for {seconds} s");
    std::thread::sleep(Duration::from_secs(seconds));
    Ok(())
}

/// Frames of `client` for `secs`: count, and for raw frames the time reading every byte took.
fn read(client: &FrameClient, secs: f64, touch: bool) -> (u32, Duration, Option<FrameLease>) {
    let (t, mut n, mut reading, mut last) = (Instant::now(), 0, Duration::ZERO, None);
    while t.elapsed().as_secs_f64() < secs {
        if let RecvOutcome::Data(f) = client.recv(Duration::from_millis(200)) {
            n += 1;
            if touch {
                let r = Instant::now();
                let sum: u64 = f
                    .planes()
                    .iter()
                    .map(|p| {
                        p.data()
                            .iter()
                            .step_by(64)
                            .map(|&b| u64::from(b))
                            .sum::<u64>()
                    })
                    .sum();
                std::hint::black_box(sum);
                reading += r.elapsed();
            }
            last = Some(f);
        }
    }
    (n, reading, last)
}

fn client(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let vision = FrameClient::options(path).request(&Frames::gray().size(1280, 800).pyramid(1))?;
    let raw = FrameClient::options(path).request(&Frames::sensor_raw())?;
    // Both at once, each on its thread.
    let r = std::thread::scope(|s| {
        let v = s.spawn(|| read(&vision, 5.0, false));
        let r = s.spawn(|| read(&raw, 5.0, true));
        (v.join().unwrap(), r.join().unwrap())
    });
    let ((vn, _, vlast), (rn, reading, rlast)) = r;
    println!("vision: {:.1} fps", f64::from(vn) / 5.0);
    println!(
        "raw: {:.1} fps, reading a frame: {:.2} ms",
        f64::from(rn) / 5.0,
        reading.as_secs_f64() * 1e3 / f64::from(rn.max(1))
    );
    if let Some(f) = &rlast {
        let fmt = f.meta().format;
        println!(
            "raw frame: {} {}x{}, {} bytes",
            fmt.code,
            fmt.resolution.width,
            fmt.resolution.height,
            f.planes().iter().map(|p| p.data().len()).sum::<usize>()
        );
    }
    if let Some(n) = vlast.as_ref().and_then(|f| f.meta().native().copied()) {
        println!(
            "vision meta: exposure {} us, gain {:.2}x{:.2}, end-start {:.2} ms, aaa {:?}",
            n.exposure_ns / 1000,
            n.analog_gain,
            n.digital_gain,
            (n.frame_end_ns as f64 - vlast.as_ref().unwrap().meta().timestamp as f64) / 1e6,
            n.aaa
        );
    } else {
        println!("vision meta: no NativeFrameMeta");
    }
    // Let the frames go: a client holding one past the service's longest hold is disconnected
    // (and the capture stops with its last client).
    drop((vlast, rlast));
    let names = [
        "exposure_time_us",
        "gain",
        "ae_flicker_mode",
        "ae_exposure_max_us",
        "ae_gain_max",
        "ae_exposure_mode",
        "ae_metering_mode",
        "ae_metering_region",
        "af_mode",
    ];
    let show = |label: &str| -> Result<(), Box<dyn std::error::Error>> {
        for d in vision.controls()? {
            if names.contains(&d.meta.name.as_str()) {
                println!(
                    "{label} {}: {:?} [{:?}, {:?}] effective [{:?}, {:?}] menu {:?}",
                    d.meta.name,
                    d.current,
                    d.meta.min,
                    d.meta.max,
                    d.effective_min,
                    d.effective_max,
                    d.meta.menu
                );
            }
        }
        Ok(())
    };
    show("before")?;
    let applied = vision.set_control(ctl::AE_EXPOSURE_MAX_US, ControlValue::Uint(2000))?;
    println!("set ae_exposure_max_us 2000: {applied:?}");
    std::thread::sleep(Duration::from_millis(500));
    // A few frames' metadata, a few frames apart: what produced each.
    for _ in 0..6 {
        let (_, _, last) = read(&vision, 0.2, false);
        if let Some(f) = last {
            let n = f.meta().native().copied().unwrap_or_default();
            println!(
                "capped frame {}: exposure {} us, gain {:.2}x{:.2}, verified {}",
                n.sequence,
                n.exposure_ns / 1000,
                n.analog_gain,
                n.digital_gain,
                n.verified
            );
        }
    }
    show("capped")?;
    let over = vision.set_control(ctl::EXPOSURE_TIME_US, ControlValue::Uint(100_000))?;
    println!("manual exposure 100000 while capped: {over:?}");
    vision.set_control(ctl::EXPOSURE_TIME_US, ControlValue::Uint(0))?;
    vision.set_control(ctl::AE_EXPOSURE_MAX_US, ControlValue::Uint(0))?;
    println!("cleared");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    match arg(0) {
        Some("serve") => serve(
            arg(1).ok_or("socket path")?,
            arg(2).and_then(|s| s.parse().ok()).unwrap_or(40),
        ),
        Some("client") => client(arg(1).ok_or("socket path")?),
        _ => Err("usage: raw_controls_check serve PATH [SECONDS] | client PATH".into()),
    }
}
