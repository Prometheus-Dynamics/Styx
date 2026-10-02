//! `--then`: the camera closed and reopened (the same or another frame rate), starting from
//! what the last session settled on, and how soon each new session has a good image.

use std::time::{Duration, Instant};

use styx_algo::{Tuning, WarmStart};
use styx_native::StreamSettings;
use styx_pipeline::device::{PispOptions, PispPipeline};
use styx_pipeline::measure::{plane_mean, settle_index};

use crate::Args;
use crate::device_run::open_camera;

const TIMEOUT: Duration = Duration::from_secs(2);
const FRAMES: usize = 45;

/// The warm start the tool's options ask for: `None` lets the pipeline recall the camera's.
pub fn requested_warm(a: &Args) -> Option<Option<WarmStart>> {
    if let Some((us, gain)) = a.start_exposure {
        let exposure = Duration::from_secs_f64(us * 1e-6);
        return Some(Some(WarmStart {
            total_exposure: exposure.as_secs_f64() * gain,
            exposure,
            analogue_gain: gain,
            colour_temperature: 4000.0,
            ..Default::default()
        }));
    }
    a.cold.then_some(None)
}

/// One session per `--then` rate; returns a summary line each.
pub fn sessions(a: &Args, tuning: &Tuning) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for &fps in &a.then {
        let (cam, opened) = open_camera(a)?;
        let settings = StreamSettings::new(1280, 800).fps(fps.round() as u32);
        let options = PispOptions::nv12_and_half_rgb(1280, 800);
        let mut p =
            PispPipeline::open(cam, &settings, tuning, options).map_err(|e| e.to_string())?;
        p.start().map_err(|e| e.to_string())?;
        let warm = p.controller().warm_start().copied();
        let o0 = p.output_format(0).ok_or("no output 0")?;
        let (w, h, s) = (o0.width as usize, o0.height as usize, o0.stride as usize);
        let mut first: Option<Instant> = None;
        let mut rows = Vec::new();
        let mut result = Ok(());
        for _ in 0..FRAMES {
            let f = match p.next(TIMEOUT) {
                Ok(f) => f,
                Err(e) => {
                    result = Err(e.to_string());
                    break;
                }
            };
            first.get_or_insert(f.dequeued);
            let y = p.output(0, &f.job).map_or(0.0, |d| plane_mean(d, w, h, s));
            rows.push((
                f.timestamp,
                f.sensor.total_exposure(),
                y,
                f.step.params.ae.locked,
                f.sensor.exposure,
                f.sensor.analogue_gain,
            ));
            p.release(&f.job);
        }
        let closed = p.close().map_err(|e| e.to_string());
        result?;
        closed?;
        let Some(first) = first else { continue };
        let to_first = first - opened;
        let total: Vec<f64> = rows.iter().map(|r| r.1).collect();
        let y: Vec<f64> = rows.iter().map(|r| r.2).collect();
        let lock = rows.iter().position(|r| r.3);
        let lock_ms = lock.map(|l| to_first + rows[l].0.saturating_sub(rows[0].0));
        out.push(format!(
            "restart at {fps} fps ({}): open -> first frame {:.1} ms, first locked frame {} ({}), exposure x gain within 5% after {} frames, output level within 5% after {} frames (first {:.1}, final {:.1}); {:.0} us x {:.2} -> {:.0} us x {:.2}",
            if warm.is_some() { "warm" } else { "cold" },
            to_first.as_secs_f64() * 1e3,
            lock.map_or("never".into(), |l| l.to_string()),
            lock_ms.map_or("-".into(), |d| format!("{:.1} ms after open", d.as_secs_f64() * 1e3)),
            settle_index(&total, 0.05).unwrap_or(0),
            settle_index(&y, 0.05).unwrap_or(0),
            y[0],
            y[y.len() - 1],
            rows[0].4.as_secs_f64() * 1e6,
            rows[0].5,
            rows[rows.len() - 1].4.as_secs_f64() * 1e6,
            rows[rows.len() - 1].5,
        ));
    }
    Ok(out)
}
