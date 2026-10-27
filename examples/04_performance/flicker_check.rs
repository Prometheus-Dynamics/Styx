//! On a device under mains-lit light: whether a native camera's AE holds still at 120 fps with
//! flicker avoidance off, and with it and deflicker on (`ae_flicker_mode`, `ae_deflicker_mode`).
//! Per setting, after 3 s to settle, 4 s of frames: the spread of exposure × gain (AE chasing
//! the flicker), of the frames' mean luma and frame to frame (flicker left in the frames), and
//! the flicker period AE detected. A last run at a fixed 1 ms exposure and gain 4 shows whether
//! the light flickers at all: an exposure near a whole flicker period averages it out, so AE
//! at its longest exposure says nothing about it.
//!
//! ```text
//! flicker_check [fps]
//! ```

use std::time::{Duration, Instant};

use styx::capture_api::native_controls as ctl;
use styx::prelude::*;

/// Mean of every 4th luma sample of every 4th row.
fn luma_mean(f: &FrameLease) -> f64 {
    let planes = f.planes();
    let y = planes[0].data();
    let res = f.meta().format.resolution;
    let (w, h) = (res.width.get() as usize, res.height.get() as usize);
    let stride = f.layout_slice()[0].stride;
    let (mut sum, mut n) = (0u64, 0u64);
    for row in (0..h).step_by(4) {
        for &v in y[row * stride..row * stride + w].iter().step_by(4) {
            sum += u64::from(v);
            n += 1;
        }
    }
    sum as f64 / n.max(1) as f64
}

/// Standard deviation of `v` relative to its mean, percent.
fn spread(v: &[f64]) -> f64 {
    let mean = v.iter().sum::<f64>() / v.len().max(1) as f64;
    let var = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / v.len().max(1) as f64;
    100.0 * var.sqrt() / mean.max(f64::EPSILON)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fps: u32 = std::env::args().nth(1).map_or(Ok(120), |a| a.parse())?;
    let camera = styx::probe_all()
        .into_iter()
        .find(|d| d.backends.iter().any(|b| b.kind == BackendKind::Native))
        .ok_or("no native camera")?;
    println!("camera: {}, {fps} fps", camera.identity.display);
    for (name, flicker, deflicker, manual) in [
        ("off", 0, 0, false),
        ("auto + deflicker", 3, 2, false),
        ("fixed 1 ms, gain 4 (is there flicker?)", 0, 0, true),
    ] {
        let plan = styx::planner::plan_frames(&camera, &Frames::nv12().size(1280, 800).fps(fps))?;
        let mut frames = plan.start()?;
        frames.set_control(ctl::AE_FLICKER_MODE, ControlValue::Int(flicker))?;
        frames.set_control(ctl::AE_DEFLICKER_MODE, ControlValue::Int(deflicker))?;
        if manual {
            frames.set_exposure_us(1000)?;
            frames.set_gain(4.0)?;
        }
        let settle = Instant::now();
        while settle.elapsed() < Duration::from_secs(3) {
            let _ = frames.next_frame(Duration::from_secs(2));
        }
        let (mut eg, mut luma) = (Vec::new(), Vec::new());
        let mut last = None;
        let mut period_ns = 0;
        let window = Instant::now();
        while window.elapsed() < Duration::from_secs(4) {
            let RecvOutcome::Data(f) = frames.next_frame(Duration::from_secs(2)) else {
                continue;
            };
            let Some(n) = f.meta().native().copied() else {
                continue;
            };
            eg.push(n.exposure_ns as f64 * f64::from(n.gain()));
            last = Some((n.exposure_ns / 1000, n.analog_gain, n.digital_gain));
            luma.push(luma_mean(&f));
            if let Some(a) = n.aaa {
                period_ns = a.flicker_period_ns;
            }
        }
        let steps: Vec<f64> = luma
            .windows(2)
            .map(|w| 100.0 * (w[1] - w[0]).abs() / w[0].max(1.0))
            .collect();
        let step = steps.iter().sum::<f64>() / steps.len().max(1) as f64;
        let mean = luma.iter().sum::<f64>() / luma.len().max(1) as f64;
        println!(
            "{name}: {} frames, mean luma {mean:.1}, last exposure/again/dgain {last:?}; exposure x \
             gain SD {:.2}%; luma SD {:.2}%, frame to frame {:.2}%; flicker period {} us",
            eg.len(),
            spread(&eg),
            spread(&luma),
            step,
            period_ns / 1000
        );
        drop(frames);
    }
    Ok(())
}
