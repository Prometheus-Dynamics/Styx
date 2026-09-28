//! What the spike measures once frames flow: a baseline run, frame rate changes, exposure
//! changes against their predicted landing frame, and one saved frame.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;

use styx_sensor::{Control, ControlRequest};

use crate::checks::rate_plans;
use crate::frames::{self, FrameSample, RunStats};
use crate::rig::Rig;
use crate::{Result, ResultExt, log};

/// Frames requested this far ahead of the current frame start, so every control's delay fits.
const LEAD: u32 = 4;

fn frame_timeout(rig: &Rig) -> Duration {
    let slowest = rig.timing.frame_duration(rig.timing.frame_length_max());
    slowest.min(Duration::from_secs(2)) + Duration::from_millis(500)
}

/// Collects `n` frames.
pub fn collect(rig: &mut Rig, n: usize) -> Result<Vec<FrameSample>> {
    let timeout = frame_timeout(rig);
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        match rig.next_frame(timeout, false)? {
            Some(f) => out.push(f.sample),
            None => {
                return Err(format!(
                    "no frame within {} ms after {} frames",
                    timeout.as_millis(),
                    out.len()
                ));
            }
        }
    }
    Ok(out)
}

/// Collects frames until one with sequence `>= last` arrives.
fn collect_until(rig: &mut Rig, last: u32) -> Result<Vec<FrameSample>> {
    let mut out = Vec::new();
    loop {
        let f = collect(rig, 1)?.remove(0);
        let done = f.sequence.wrapping_sub(last) < u32::MAX / 2;
        out.push(f);
        if done {
            return Ok(out);
        }
    }
}

fn report(label: &str, s: &RunStats) {
    log!(
        "{label}: {} frames, {:.3} fps, {} missing (largest gap {}), {} with errors, mean level {:.1}",
        s.frames,
        s.fps,
        s.missing,
        s.largest_gap,
        s.errors,
        s.mean
    );
}

/// The frame to request changes for: a few frames past the current frame start.
fn target_frame(rig: &mut Rig) -> Result<u32> {
    if rig.current_frame().is_none() {
        collect(rig, 1)?;
    }
    Ok(rig.current_frame().unwrap_or(0) + LEAD)
}

/// Streams `n` frames and reports rate, gaps and level.
pub fn baseline(rig: &mut Rig, n: usize) -> Result<RunStats> {
    let frames = collect(rig, n)?;
    let s = RunStats::of(&frames);
    let t = &rig.timing;
    let expected = t.fps(t.frame_length_default());
    report("baseline", &s);
    log!("baseline: expected {expected:.3} fps at the default frame length");
    Ok(s)
}

/// Sets a frame rate through the timing model and the control scheduler; returns the frame it
/// is predicted to land on and the predicted rate.
fn set_rate(rig: &mut Rig, fps: f64) -> Result<(u32, f64)> {
    let plan = rate_plans(&rig.timing, &[fps])[0];
    let frame = target_frame(rig)?;
    let landing = {
        let mut d = rig.driver();
        let duration = rig.timing.frame_duration(plan.frame_length);
        d.request(
            u64::from(frame),
            &ControlRequest {
                frame_duration: Some(duration),
                ..Default::default()
            },
        )
        .ctx("request frame duration")?
    };
    let lands = landing
        .iter()
        .find(|l| l.control == Control::FrameLength)
        .map_or(u64::from(frame), |l| l.frame) as u32;
    // Tell readers of the bridge controls (informational; the sensor register does the work).
    if let Err(e) = rig
        .bridge
        .set_blanking(rig.timing.hblank as i32, plan.vblank as i32)
    {
        log!("bridge VBLANK: {e}");
    }
    log!(
        "rate {fps}: frame length {} (vblank {}), predicted {:.3} fps{}, requested for frame {frame}, lands on {lands}",
        plan.frame_length,
        plan.vblank,
        plan.fps,
        if plan.clamped { " (clamped)" } else { "" }
    );
    Ok((lands, plan.fps))
}

/// For each target rate: request it, wait for it to land, measure about a second of frames.
pub fn fps_check(rig: &mut Rig, targets: &[f64]) -> Result<Vec<(f64, f64, f64)>> {
    let mut results = Vec::new();
    for &fps in targets {
        let (lands, predicted) = set_rate(rig, fps)?;
        let around = collect_until(rig, lands + 1)?;
        let n = (predicted.ceil() as usize).clamp(20, 240);
        let frames = collect(rig, n)?;
        let s = RunStats::of(&frames);
        report(&format!("rate {fps}"), &s);
        // The first frame whose period matches the new rate (within 5%).
        let period = 1.0 / predicted;
        let first_new = around
            .windows(2)
            .chain(frames.windows(2).take(3))
            .find(|w| {
                let dt = (w[1].timestamp.saturating_sub(w[0].timestamp)).as_secs_f64()
                    / f64::from(w[1].sequence.wrapping_sub(w[0].sequence).max(1));
                (dt - period).abs() / period < 0.05
            })
            .map(|w| w[1].sequence);
        log!(
            "rate {fps}: measured {:.3} fps vs predicted {predicted:.3} ({:+.2}%), first frame at the new period: {} (predicted {lands})",
            s.fps,
            (s.fps - predicted) / predicted * 100.0,
            first_new.map_or("?".into(), |f| f.to_string())
        );
        results.push((fps, predicted, s.fps));
    }
    Ok(results)
}

/// Exposure steps (up then back down, or down then up in a bright scene) at `fps`; reports on
/// which frame the mean level moved against the predicted landing frame.
pub fn exposure_check(rig: &mut Rig, fps: f64) -> Result<Vec<(u32, Option<u32>)>> {
    let (lands, _) = set_rate(rig, fps)?;
    collect_until(rig, lands + 2)?;
    let mut results = Vec::new();
    let settle = collect(rig, 8)?;
    let mut before = RunStats::of(&settle).mean;
    let seq = settle.last().map_or(0, |f| f.sequence);
    let (mut exposure, frame_length) = {
        let d = rig.driver();
        let a = d.applied(u64::from(seq)).ok_or("no applied controls")?;
        (a.exposure, a.frame_length)
    };
    let limits = rig.timing.exposure_limits(frame_length);
    let headroom = 1023.0 - rig.black;
    let up_first = before - rig.black < headroom * 0.3;
    log!(
        "exposure: level {before:.1} (black {:.0}) at {:.3} ms, limits {:.3}..{:.3} ms",
        rig.black,
        exposure.as_secs_f64() * 1e3,
        limits.min.as_secs_f64() * 1e3,
        limits.max.as_secs_f64() * 1e3
    );
    if before - rig.black < 4.0 {
        log!(
            "exposure: the scene is black at this exposure (lens covered?): results will be inconclusive"
        );
    }
    for step in 0..2 {
        let up = (step == 0) == up_first;
        let target = if up {
            (exposure * 2).min(limits.max)
        } else {
            (exposure / 2).max(limits.min)
        };
        let ratio = target.as_secs_f64() / exposure.as_secs_f64();
        let expected = frames::expected_level(before, rig.black, ratio);
        let frame = target_frame(rig)?;
        let landing = rig
            .driver()
            .request(
                u64::from(frame),
                &ControlRequest {
                    exposure: Some(target),
                    ..Default::default()
                },
            )
            .ctx("request exposure")?;
        let predicted = landing
            .iter()
            .find(|l| l.control == Control::Exposure)
            .map_or(u64::from(frame), |l| l.frame) as u32;
        let frames = collect_until(rig, predicted + 6)?;
        for f in &frames {
            let applied = rig.driver().applied(u64::from(f.sequence));
            log!(
                "  frame {:>5} level {:>7.1}  predicted exposure {:>8.3} ms{}",
                f.sequence,
                f.mean,
                applied.map_or(0.0, |a| a.exposure.as_secs_f64() * 1e3),
                if f.sequence == predicted {
                    "  <- predicted landing"
                } else {
                    ""
                }
            );
        }
        let observed =
            frames::level_change_frame(&frames, frame.saturating_sub(LEAD), before, expected);
        log!(
            "exposure {:.3} -> {:.3} ms (requested for frame {frame}): level {before:.1} -> expected {expected:.1}; predicted landing {predicted}, observed {}{}",
            exposure.as_secs_f64() * 1e3,
            target.as_secs_f64() * 1e3,
            observed.map_or("none".into(), |o| o.to_string()),
            observed.map_or(String::new(), |o| format!(
                " ({:+} frames)",
                i64::from(o) - i64::from(predicted)
            ))
        );
        results.push((predicted, observed));
        exposure = target;
        before = RunStats::of(&frames[frames.len().saturating_sub(3)..]).mean;
    }
    Ok(results)
}

/// Saves the next frame as raw bytes and as an 8-bit grey PGM (2x2 cells averaged).
pub fn save_frame(rig: &mut Rig, dir: &Path) -> Result<()> {
    let timeout = frame_timeout(rig);
    let f = rig.next_frame(timeout, true)?.ok_or("no frame to save")?;
    let data = f.data.ok_or("no frame data")?;
    let l = rig.layout;
    let stem = format!(
        "native-spike-{}x{}-{}-{}",
        l.width, l.height, rig.fourcc, f.sample.sequence
    );
    let raw = dir.join(format!("{stem}.raw"));
    std::fs::write(&raw, &data).ctx("write raw frame")?;
    let (w, h, grey) = l.grey_half(&data).ctx("grey image")?;
    let pgm = dir.join(format!("{stem}.pgm"));
    let mut out = BufWriter::new(File::create(&pgm).ctx("create PGM")?);
    frames::write_pgm(&mut out, w, h, &grey).ctx("write PGM")?;
    out.flush().ctx("write PGM")?;
    log!(
        "saved frame {}: {} ({} bytes, stride {}), {} ({w}x{h} grey), level {:.1}",
        f.sample.sequence,
        raw.display(),
        data.len(),
        l.stride,
        pgm.display(),
        f.sample.mean
    );
    Ok(())
}
