//! Checks that exposure, gain and frame duration land on the frames the control schedule
//! predicts, from the raw image levels (no embedded data needed): every frame's mean level
//! above black, divided by the exposure × gain the frame reports, must stay the same, also on
//! the frames where a change lands. Works on bridged and kernel driver's sensors alike.
//!
//! ```sh
//! landing [fps] [rounds] [key]
//! ```
//!
//! Each round steps the exposure (×2, back), the gain (×2, back), both at once the other way
//! (exposure ×2, gain ÷2: the level must not move), and the frame duration (the rate ÷1.5,
//! back); half the requests are written at once within the current frame
//! (`request_at_now`, as the 3A loop does), half at the next frame starts (`request`).

use std::time::Duration;

use styx_native::styx_sensor::ControlRequest;
use styx_native::{CameraControls, NativeCamera, NativeError, NativeFrame, SensorLibrary};
use styx_native::{FrameStream, StreamSettings};

const TIMEOUT: Duration = Duration::from_secs(2);

/// One frame: sequence, timestamp (frame start on `rp1-cfe`), level above black (0..1), the
/// exposure × gain (µs) and frame duration (ms) it reports.
#[derive(Clone, Copy, Debug)]
struct Row {
    seq: u64,
    ts: Duration,
    level: f64,
    product: f64,
    period_ms: f64,
}

fn level(f: &NativeFrame, black: f64) -> f64 {
    let d = f.data();
    let (w, stride) = (f.width as usize, f.stride as usize);
    let packed10 = stride >= w * 5 / 4 && stride < w * 2;
    let (mut sum, mut n) = (0f64, 0f64);
    for row in (0..f.height as usize).step_by(4) {
        let line = &d[row * stride..];
        if packed10 {
            for g in line[..w * 5 / 4].chunks_exact(5).step_by(2) {
                for (i, &b) in g[..4].iter().enumerate() {
                    sum += f64::from((u16::from(b) << 2) | u16::from((g[4] >> (2 * i)) & 3));
                    n += 1.0;
                }
            }
        } else {
            for &b in line[..w].iter().step_by(4) {
                sum += f64::from(b) * 4.0;
                n += 1.0;
            }
        }
    }
    ((sum / n.max(1.0)) - black) / (1023.0 - black)
}

fn next(s: &mut FrameStream, black: f64) -> Result<Row, NativeError> {
    let f = s.next_blocking(TIMEOUT)?.ok_or(NativeError::Timeout)?;
    let c = f.controls.ok_or(NativeError::State("no frame values"))?;
    Ok(Row {
        seq: u64::from(f.sequence),
        ts: f.timestamp,
        level: level(&f, black),
        product: c.exposure.as_secs_f64() * 1e6 * c.gain(),
        period_ms: c.frame_duration.as_secs_f64() * 1e3,
    })
}

/// Sends a request; returns the frame it lands on.
fn ask(c: &CameraControls, req: ControlRequest, now: bool) -> Result<u64, NativeError> {
    let landed = if now {
        c.request_at_now(0, &req)?
    } else {
        c.request(&req)?
    };
    Ok(landed.iter().map(|l| l.frame).max().unwrap_or(0))
}

fn main() -> Result<(), NativeError> {
    let args: Vec<String> = std::env::args().collect();
    let fps: f64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(30.0);
    let rounds: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(4);
    let (cameras, problems) = styx_native::discover(&SensorLibrary::system());
    for p in problems {
        println!("problem: {p}");
    }
    let info = match args.get(3) {
        Some(k) => cameras.into_iter().find(|c| &c.key == k),
        None => cameras.into_iter().next(),
    }
    .ok_or(NativeError::State("no camera"))?;
    println!(
        "{} [{}]: {}",
        info.display_name(),
        info.key,
        info.description_source
    );
    let black = info
        .description
        .pixel_array
        .black_level
        .map_or(0.0, |b| b.at_bits(10));
    let mut cam = NativeCamera::open(info, Default::default())?;
    let cfg = cam.configure(&StreamSettings::new(1280, 800).fps(fps.round() as u32))?;
    let c = cam.controls();
    let d = |ctl| c.delay(ctl);
    use styx_native::styx_sensor::Control::*;
    println!(
        "{:.3} fps, delays exposure {} gain {} frame length {}",
        cfg.interval.fps(),
        d(Exposure),
        d(AnalogGain),
        d(FrameLength)
    );
    let frame = Duration::from_secs_f64(1.0 / fps);
    let mut s = cam.start()?;
    // A base exposure that leaves room for ×2 below saturation.
    let mut e = (frame / 4).min(Duration::from_millis(8));
    let mut g = 1.0;
    c.set_exposure(e)?;
    c.set_gain(g)?;
    let mut rows = Vec::new();
    for _ in 0..8 {
        rows.push(next(&mut s, black)?);
    }
    let lvl = rows.last().map_or(0.1, |r| r.level).max(1e-3);
    // Gain 2 at least (the gain step halves it), the exposure at most 0.4 frames (it doubles).
    let total = e.as_secs_f64() * g * (0.2 / lvl).clamp(0.05, 16.0);
    g = 2.0;
    e = Duration::from_secs_f64(total / g);
    if e > frame.mul_f64(0.4) {
        e = frame.mul_f64(0.4);
        g = (total / e.as_secs_f64()).min(7.0);
    }
    ask(
        &c,
        ControlRequest {
            exposure: Some(e),
            gain: Some(g),
            ..Default::default()
        },
        false,
    )?;
    for _ in 0..6 {
        rows.push(next(&mut s, black)?);
    }
    println!(
        "base: exposure {:.0} us, gain {g:.2}",
        e.as_secs_f64() * 1e6
    );
    let slow = Duration::from_secs_f64(1.5 / fps);
    let mut steps = Vec::new();
    let mut now = false;
    for _ in 0..rounds {
        let plan = [
            ("exposure x2", Some(e * 2), None, None),
            ("exposure back", Some(e), None, None),
            ("gain x2", None, Some(g * 2.0), None),
            ("gain back", None, Some(g), None),
            ("exposure x2, gain /2", Some(e * 2), Some(g / 2.0), None),
            ("both back", Some(e), Some(g), None),
            ("rate /1.5", None, None, Some(slow)),
            ("rate back", None, None, Some(frame)),
        ];
        for (what, exposure, gain, fd) in plan {
            let req = ControlRequest {
                exposure,
                gain,
                frame_duration: fd,
            };
            let at = rows.last().map_or(0, |r| r.seq);
            let lands = ask(&c, req, now)?;
            steps.push((what, at, lands, now));
            now = !now;
            for _ in 0..6 {
                rows.push(next(&mut s, black)?);
            }
        }
    }
    cam.stop()?;
    drop(s);
    report(&rows, &steps);
    cam.close()
}

fn report(rows: &[Row], steps: &[(&str, u64, u64, bool)]) {
    // Skip the start and the base settling (the first frame reads a higher black level).
    let settled: Vec<&Row> = rows.iter().skip(14).collect();
    let mut ratios: Vec<f64> = settled.iter().map(|r| r.level / r.product).collect();
    ratios.sort_by(f64::total_cmp);
    let median = ratios[ratios.len() / 2];
    let off: Vec<&&Row> = settled
        .iter()
        .filter(|r| ((r.level / r.product) / median - 1.0).abs() > 0.05)
        .collect();
    println!(
        "{} frames: level / (exposure x gain) within 5% of its median on {} ({} off)",
        settled.len(),
        settled.len() - off.len(),
        off.len()
    );
    for r in &off {
        println!(
            "  off: frame {} level {:.4} predicted {:.0} us ratio {:+.1}%",
            r.seq,
            r.level,
            r.product,
            ((r.level / r.product) / median - 1.0) * 100.0
        );
    }
    // Measured frame periods (start to next start) against the reported frame durations.
    let measured = |s: u64| -> Option<f64> {
        let a = rows.iter().find(|r| r.seq == s)?;
        let b = rows.iter().find(|r| r.seq == s + 1)?;
        Some((b.ts - a.ts).as_secs_f64() * 1e3)
    };
    let periods: Vec<(u64, f64, f64)> = settled
        .iter()
        .filter_map(|r| Some((r.seq, measured(r.seq)?, r.period_ms)))
        .collect();
    let wrong: Vec<&(u64, f64, f64)> = periods
        .iter()
        .filter(|(_, m, p)| (m / p - 1.0).abs() > 0.01)
        .collect();
    println!(
        "{} frame periods: within 1% of the reported frame duration on {} ({} off)",
        periods.len(),
        periods.len() - wrong.len(),
        wrong.len()
    );
    for (seq, m, p) in wrong {
        println!("  off: frame {seq} lasted {m:.3} ms, reported {p:.3} ms");
    }
    let mut hits = 0;
    for (what, at, lands, now) in steps {
        let by = |s: u64| rows.iter().find(|r| r.seq == s);
        let before = by(lands.saturating_sub(1));
        let landed = by(*lands);
        let (Some(b), Some(l)) = (before, landed) else {
            continue;
        };
        // Where the level (or, for rate steps, the frame period) changed first.
        let changed = rows
            .iter()
            .filter(|r| r.seq > *at && r.seq <= lands + 3)
            .find(|r| {
                if what.starts_with("rate") {
                    let (Some(m), Some(mb)) = (measured(r.seq), measured(b.seq)) else {
                        return false;
                    };
                    (m / mb - 1.0).abs() > 0.2
                } else {
                    (r.level / b.level - 1.0).abs() > 0.25
                }
            })
            .map(|r| r.seq);
        // A step that keeps the level (exposure ×2, gain ÷2) shows a frame ×2 or ÷2 off its
        // prediction when the two land apart; light flicker moves it by a few percent.
        let split = rows
            .iter()
            .filter(|r| r.seq + 1 >= *lands && r.seq <= lands + 1)
            .any(|r| ((r.level / r.product) / median - 1.0).abs() > 0.25);
        let ok = match changed {
            Some(s) => s == *lands,
            None => !split && (l.level / b.level - 1.0).abs() < 0.25 && !what.starts_with("rate"),
        };
        hits += usize::from(ok);
        println!(
            "  {what:22} asked in frame {at:4} ({}), predicted {lands:4} (+{}), changed {:>5} {}",
            if *now { "now " } else { "next" },
            lands - at,
            changed.map_or("-".into(), |c| c.to_string()),
            if ok { "ok" } else { "MISSED" }
        );
    }
    println!(
        "{hits} of {} steps landed on the predicted frame",
        steps.len()
    );
}
