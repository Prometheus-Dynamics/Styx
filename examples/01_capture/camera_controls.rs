//! Camera controls and per-frame metadata.
//!
//! Native cameras (sensors Styx drives), processed NV12 with 3A in Rust:
//! * auto exposure until AE converges (`AE_STATE`), then manual exposure and gain, exposure
//!   compensation, AE back on;
//! * white balance: AWB's estimate, a fixed colour temperature, manual red/blue gains;
//! * denoise: a capture setting (`StyxConfig::native_temporal_denoise`, `native_spatial_denoise`);
//! * frame rate: a processed capture keeps its rate, so it is restarted at another
//!   (`CaptureHandle::reconfigure`).
//!
//! Native raw frames: exposure and frame rate written by the control schedule, and the frame
//! each change landed on, read from every frame's metadata (`NativeFrameMeta`: the exposure,
//! gains and frame duration that produced it, read back from the sensor's embedded data where
//! it has some).
//!
//! V4L2 cameras: their controls by name (UVC: brightness, exposure, ...), set and read back.
//!
//! ```sh
//! cargo run -p styx-examples --features native,v4l2 --bin camera_controls
//! ```

use std::time::{Duration, Instant};

use styx::capture_api::native_controls as ctl;
use styx::prelude::*;

fn next(handle: &CaptureHandle) -> Option<FrameLease> {
    match handle.recv_blocking(Duration::from_secs(2)) {
        RecvOutcome::Data(frame) => Some(frame),
        _ => None,
    }
}

/// One line of what produced a frame.
fn describe(frame: &FrameLease) -> String {
    let meta = frame.meta();
    match meta.native() {
        Some(m) => format!(
            "frame {:>4} t={:.4} s: exposure {:6.3} ms, gain {:.2} (analogue {:.2} x digital {:.2}), frame {:.2} ms{}",
            m.sequence,
            meta.timestamp as f64 / 1e9,
            m.exposure_ns as f64 / 1e6,
            m.gain(),
            m.analog_gain,
            m.digital_gain,
            m.frame_duration_ns as f64 / 1e6,
            if m.verified {
                " (read back)"
            } else {
                " (predicted)"
            }
        ),
        None => format!("t={:.4} s", meta.timestamp as f64 / 1e9),
    }
}

/// Waits for the first frame whose exposure (and gain, when given) matches; prints how many
/// frames after the request it was.
fn wait_for(handle: &CaptureHandle, exposure_us: u32, gain: Option<f32>) {
    let asked_after = next(handle).and_then(|f| f.meta().sequence());
    for _ in 0..30 {
        let Some(frame) = next(handle) else { return };
        let Some(m) = frame.meta().native() else {
            return;
        };
        let exposure_ok = (m.exposure_ns as f64 / 1e3 - f64::from(exposure_us)).abs()
            < 0.02 * f64::from(exposure_us);
        if exposure_ok && gain.is_none_or(|g| (m.gain() - g).abs() < 0.05 * g) {
            let after = asked_after.map_or(0, |s| m.sequence.wrapping_sub(s));
            println!(
                "  landed {after} frame(s) after the request: {}",
                describe(&frame)
            );
            return;
        }
    }
    println!("  did not land within 30 frames");
}

fn ae_state(handle: &CaptureHandle) -> &'static str {
    match handle.get_control(ctl::AE_STATE) {
        Ok(ControlValue::Int(2)) => "converged",
        Ok(_) => "searching",
        Err(_) => "-",
    }
}

fn find_mode(backend: &ProbedBackend, code: FourCc) -> Option<ModeId> {
    backend
        .descriptor
        .modes
        .iter()
        .find(|m| m.format.code == code)
        .map(|m| m.id.clone())
}

fn processed(device: &ProbedDevice) -> Result<(), CaptureError> {
    let backend = device.backend(BackendKind::Native).expect("native");
    let Some(mode) = find_mode(backend, FourCc::NV12) else {
        return Ok(());
    };
    println!("\n== processed NV12, 3A in Rust ==");
    // Denoise is a capture setting of the ISP (temporal denoise on: libcamera's quality, about
    // 1.5 ms more ISP time per frame).
    let config = StyxConfig::new()
        .native_temporal_denoise(true)
        .native_spatial_denoise(100);
    let request = |fps: u32| {
        CaptureRequest::new(device)
            .backend(BackendKind::Native)
            .mode(mode.clone())
            .interval(Interval::from_fps(fps).expect("fps"))
            .config(config.clone())
    };
    let opened = Instant::now();
    let mut handle = request(30).start()?;
    let mut frames = 0;
    while let Some(frame) = next(&handle) {
        frames += 1;
        if matches!(handle.get_control(ctl::AE_STATE), Ok(ControlValue::Int(2))) || frames == 90 {
            // AE that cannot reach its target (a dark scene: exposure and gain at their limits)
            // keeps searching.
            println!(
                "AE {} at frame {frames}, {:.0} ms after open: {}",
                ae_state(&handle),
                opened.elapsed().as_secs_f64() * 1e3,
                describe(&frame)
            );
            break;
        }
    }
    println!("AWB: {:?} K", handle.get_control(ctl::COLOUR_TEMPERATURE)?);

    println!("manual: exposure 10 ms, gain 2.0");
    handle.set_control(ctl::EXPOSURE_TIME_US, ControlValue::Uint(10_000))?;
    handle.set_control(ctl::GAIN, ControlValue::Float(2.0))?;
    wait_for(&handle, 10_000, Some(2.0));

    println!("exposure fixed at 5 ms, gain automatic (0), +1 stop of exposure compensation");
    handle.set_control(ctl::EXPOSURE_TIME_US, ControlValue::Uint(5_000))?;
    handle.set_control(ctl::GAIN, ControlValue::Float(0.0))?;
    handle.set_control(ctl::EXPOSURE_VALUE, ControlValue::Float(1.0))?;
    wait_for(&handle, 5_000, None);
    for _ in 0..20 {
        next(&handle);
    }
    let frame = next(&handle).ok_or(CaptureError::Disconnected("no frame".into()))?;
    println!("  AE {}: {}", ae_state(&handle), describe(&frame));

    println!("white balance: AWB off, 5000 K; then red 1.8 / blue 1.4");
    handle.set_control(ctl::AWB_ENABLE, ControlValue::Bool(false))?;
    handle.set_control(ctl::COLOUR_TEMPERATURE, ControlValue::Uint(5000))?;
    for _ in 0..5 {
        next(&handle);
    }
    println!(
        "  colour temperature now {:?}",
        handle.get_control(ctl::COLOUR_TEMPERATURE)?
    );
    handle.set_control(ctl::RED_GAIN, ControlValue::Float(1.8))?;
    handle.set_control(ctl::BLUE_GAIN, ControlValue::Float(1.4))?;
    println!(
        "  gains red {:?} blue {:?}",
        handle.get_control(ctl::RED_GAIN)?,
        handle.get_control(ctl::BLUE_GAIN)?
    );

    println!("everything automatic again");
    for (id, v) in [
        (ctl::EXPOSURE_TIME_US, ControlValue::Uint(0)),
        (ctl::EXPOSURE_VALUE, ControlValue::Float(0.0)),
        (ctl::AWB_ENABLE, ControlValue::Bool(true)),
    ] {
        handle.set_control(id, v)?;
    }

    match handle.set_control(ctl::FRAME_RATE, ControlValue::Float(60.0)) {
        Ok(()) => println!("frame rate set"),
        Err(e) => println!("frame rate control: {e}"),
    }
    let t = Instant::now();
    handle.reconfigure_in_place(request(60))?;
    let first = next(&handle);
    println!(
        "restarted at 60 fps: first frame {:.0} ms after the restart began",
        t.elapsed().as_secs_f64() * 1e3
    );
    if let Some(frame) = first {
        println!("  {}", describe(&frame));
    }
    handle.stop();
    Ok(())
}

fn raw(device: &ProbedDevice) -> Result<(), CaptureError> {
    let backend = device.backend(BackendKind::Native).expect("native");
    let Some(mode) = backend
        .descriptor
        .modes
        .iter()
        .find(|m| !matches!(m.format.code, FourCc::NV12 | FourCc::RG24))
        .map(|m| m.id.clone())
    else {
        return Ok(());
    };
    println!(
        "\n== raw {}, controls straight to the sensor ==",
        mode.format.code
    );
    let handle = CaptureRequest::new(device)
        .backend(BackendKind::Native)
        .mode(mode)
        .interval(Interval::from_fps(30).expect("fps"))
        .control(ctl::EXPOSURE_TIME_US, ControlValue::Uint(2_000))
        .start()?;
    for _ in 0..5 {
        next(&handle);
    }
    println!("exposure 6 ms, gain 3.0");
    handle.set_control(ctl::EXPOSURE_TIME_US, ControlValue::Uint(6_000))?;
    handle.set_control(ctl::GAIN, ControlValue::Float(3.0))?;
    wait_for(&handle, 6_000, Some(3.0));
    println!("frame rate 60 fps");
    handle.set_control(ctl::FRAME_RATE, ControlValue::Float(60.0))?;
    let mut last: Option<FrameLease> = None;
    for _ in 0..8 {
        let Some(frame) = next(&handle) else { break };
        if let Some(prev) = &last {
            let dt = frame.meta().timestamp.saturating_sub(prev.meta().timestamp);
            println!(
                "  {} (interval {:.2} ms)",
                describe(&frame),
                dt as f64 / 1e6
            );
        }
        last = Some(frame);
    }
    handle.stop();
    Ok(())
}

fn v4l2(device: &ProbedDevice) -> Result<(), CaptureError> {
    let Some(backend) = device.backend(BackendKind::V4l2) else {
        return Ok(());
    };
    println!("\n== V4L2: {} ==", device.identity.display);
    let handle = CaptureRequest::new(device)
        .backend(BackendKind::V4l2)
        .start()?;
    next(&handle);
    for c in &backend.descriptor.controls {
        println!("  {:<36} now {:?}", c.name, handle.get_control(c.id).ok());
    }
    // Controls by name, as the driver calls them.
    if let Some(c) = backend
        .descriptor
        .controls
        .iter()
        .find(|c| c.name.to_lowercase().contains("brightness"))
    {
        let before = handle.get_control(c.id)?;
        handle.set_control(c.id, c.max.clone())?;
        println!(
            "  {}: {before:?} -> {:?}",
            c.name,
            handle.get_control(c.id)?
        );
        handle.set_control(c.id, before)?;
    }
    handle.stop();
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let devices = styx::probe_all();
    let mut ran = false;
    for device in &devices {
        if device.backend(BackendKind::Native).is_some() {
            println!("{}", device.identity.display);
            processed(device)?;
            raw(device)?;
            ran = true;
        } else if device.backend(BackendKind::V4l2).is_some() {
            v4l2(device)?;
            ran = true;
        }
    }
    if !ran {
        println!("no native or V4L2 camera (enable the `native` / `v4l2` features)");
    }
    Ok(())
}
