//! `latch`: when, within a frame, a register write still lands `delay` frames later.
//!
//! AE settles, then is held. For each offset after a frame's start (`FRAME_SYNC` time, which
//! the raw frame's timestamp carries), the exposure is halved by a write issued at once (group
//! hold, as every control write), and the frames' embedded data say which frame first used
//! it. The write is counted in the frame that had started when it went out.

use std::time::Duration;

use styx_algo::Controls;
use styx_native::styx_sensor::ControlRequest;
use styx_pipeline::device::{PispOptions, PispPipeline};

use crate::device_run::{open_camera, settings};
use crate::{Args, monotonic, tuning};

const TIMEOUT: Duration = Duration::from_secs(2);

pub fn run(a: &Args) -> Result<(), String> {
    let tuning = tuning(a)?;
    let (cam, _) = open_camera(a)?;
    let options = PispOptions::nv12_and_half_rgb(1280, 800);
    let mut p =
        PispPipeline::open(cam, &settings(a), &tuning, options).map_err(|e| e.to_string())?;
    p.start().map_err(|e| e.to_string())?;
    let result = measure(&mut p);
    let closed = p.close().map_err(|e| e.to_string());
    result.and(closed)
}

fn next(p: &mut PispPipeline) -> Result<(u64, Duration, Duration, bool), String> {
    let f = p.next(TIMEOUT).map_err(|e| e.to_string())?;
    p.release(&f.job);
    Ok((
        f.sequence,
        f.timestamp,
        f.sensor.exposure,
        f.sensor.verified,
    ))
}

fn measure(p: &mut PispPipeline) -> Result<(), String> {
    let mut last = next(p)?;
    for _ in 0..40 {
        last = next(p)?;
    }
    let (e0, g0) = {
        let f = p.controls().applied(last.0).ok_or("no values")?;
        (f.exposure, f.analog_gain)
    };
    p.controller().set_controls(Controls {
        ae_enable: false,
        exposure: Some(e0),
        analogue_gain: Some(g0),
        ..Default::default()
    });
    for _ in 0..6 {
        last = next(p)?;
    }
    let period = p
        .controls()
        .applied(last.0)
        .ok_or("no values")?
        .frame_duration;
    println!(
        "latch: exposure {:.0} us, gain {g0:.2}, frame {:.3} ms",
        e0.as_secs_f64() * 1e6,
        period.as_secs_f64() * 1e3
    );
    p.controls().set_write_margin(Some(Duration::ZERO));
    let e1 = e0 / 2;
    let offsets_ms = [
        1.0, 3.0, 6.0, 9.0, 12.0, 16.0, 20.0, 24.0, 27.0, 29.0, 30.5, 31.5, 32.3, 32.8,
    ];
    let mut rows = Vec::new();
    for off in offsets_ms {
        for (value, what) in [(e1, "half"), (e0, "back")] {
            // The frame start to aim from: the last frame's, moved on by whole periods until
            // the offset is still ahead.
            let mut start = last.1;
            let at = loop {
                let t = start + Duration::from_secs_f64(off * 1e-3);
                if t > monotonic() + Duration::from_micros(300) {
                    break t;
                }
                start += period;
            };
            while monotonic() < at {
                std::hint::spin_loop();
            }
            let wrote = monotonic();
            let landed = p
                .controls()
                .request_at_now(
                    0,
                    &ControlRequest {
                        exposure: Some(value),
                        ..Default::default()
                    },
                )
                .map_err(|e| e.to_string())?;
            let done = monotonic();
            let predicted = landed.iter().map(|l| l.frame).max().unwrap_or(0);
            // Frames until the new exposure shows up in the embedded data.
            let mut frames = Vec::new();
            let seen = loop {
                let f = next(p)?;
                frames.push(f);
                let close =
                    (f.2.as_secs_f64() - value.as_secs_f64()).abs() < 0.02 * value.as_secs_f64();
                if f.3 && close {
                    break f.0;
                }
                if frames.len() > 8 {
                    break u64::MAX;
                }
            };
            last = *frames.last().expect("one frame");
            // The frame the write went out in: the last one that started before it.
            let in_frame = frames
                .iter()
                .rev()
                .find(|f| f.1 <= wrote)
                .map(|f| (f.0, wrote - f.1));
            let (s, into) = in_frame.unwrap_or((predicted.saturating_sub(2), Duration::ZERO));
            println!(
                "offset {off:5.1} ms ({what}): written {:.2} ms into frame {s} ({:.2} ms of writes), predicted {predicted}, seen on {} (+{})",
                into.as_secs_f64() * 1e3,
                (done - wrote).as_secs_f64() * 1e3,
                if seen == u64::MAX {
                    "never".into()
                } else {
                    seen.to_string()
                },
                seen.saturating_sub(s)
            );
            rows.push((off, seen.saturating_sub(s)));
            for _ in 0..3 {
                last = next(p)?;
            }
        }
    }
    p.controls()
        .set_write_margin(Some(styx_native::control::DEFAULT_WRITE_MARGIN));
    Ok(())
}
