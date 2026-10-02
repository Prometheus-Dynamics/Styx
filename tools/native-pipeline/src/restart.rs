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

/// One session per `--then` rate, reopening the camera each time; returns a summary line each.
pub fn sessions(a: &Args, tuning: &Tuning) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for &fps in &a.then {
        let (cam, opened) = open_camera(a)?;
        let camera_open = opened.elapsed();
        let settings = StreamSettings::new(1280, 800).fps(fps.round() as u32);
        let options = PispOptions::nv12_and_half_rgb(1280, 800);
        let mut p =
            PispPipeline::open(cam, &settings, tuning, options).map_err(|e| e.to_string())?;
        p.start().map_err(|e| e.to_string())?;
        let rows = frames(&mut p);
        let startup = *p.startup();
        let closed = p.close().map_err(|e| e.to_string());
        let rows = rows?;
        out.push(summary(&format!("reopened at {fps} fps"), opened, &rows));
        out.push(format!(
            "  {}",
            crate::device_run::startup_line(startup, camera_open, rows.first().map(|r| r.0))
        ));
        closed?;
    }
    Ok(out)
}

/// The `--then` rates on the camera kept open and powered (`--keep-open`): stop, reconfigure
/// when the rate changes, start again. Closes the pipeline.
pub fn in_place(a: &Args, mut p: PispPipeline) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut fps_now = a.fps;
    let mut result = Ok(());
    for &fps in &a.then {
        let t0 = Instant::now();
        if fps != fps_now {
            let settings = StreamSettings::new(1280, 800).fps(fps.round() as u32);
            if let Err(e) = p.reconfigure(&settings) {
                result = Err(e.to_string());
                break;
            }
            fps_now = fps;
        }
        if let Err(e) = p.start() {
            result = Err(e.to_string());
            break;
        }
        let rows = frames(&mut p);
        let stopped = p.stop();
        match rows {
            Ok(r) => out.push(summary(&format!("restarted in place at {fps} fps"), t0, &r)),
            Err(e) => {
                result = Err(e);
                break;
            }
        }
        if let Err(e) = stopped {
            result = Err(e.to_string());
            break;
        }
    }
    let closed = p.close().map_err(|e| e.to_string());
    result?;
    closed?;
    Ok(out)
}

type Row = (Instant, Duration, f64, f64, bool, Duration, f64, bool);

/// Runs a started pipeline for [`FRAMES`] frames: (dequeued, timestamp, exposure x gain,
/// output mean, locked, exposure, gain, warm).
fn frames(p: &mut PispPipeline) -> Result<Vec<Row>, String> {
    let warm = p.controller().warm_start().is_some();
    let o0 = p.output_format(0).ok_or("no output 0")?;
    let (w, h, s) = (o0.width as usize, o0.height as usize, o0.stride as usize);
    let mut rows = Vec::new();
    for _ in 0..FRAMES {
        let f = p.next(TIMEOUT).map_err(|e| e.to_string())?;
        p.sync_output(0, &f.job, true).map_err(|e| e.to_string())?;
        let y = p.output(0, &f.job).map_or(0.0, |d| plane_mean(d, w, h, s));
        p.sync_output(0, &f.job, false).map_err(|e| e.to_string())?;
        rows.push((
            f.dequeued,
            f.timestamp,
            f.sensor.total_exposure(),
            y,
            p.step().params.ae.locked,
            f.sensor.exposure,
            f.sensor.analogue_gain,
            warm,
        ));
        p.release(&f.job);
    }
    Ok(rows)
}

fn summary(what: &str, t0: Instant, rows: &[Row]) -> String {
    let Some(first) = rows.first() else {
        return format!("{what}: no frames");
    };
    let last = &rows[rows.len() - 1];
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    let to_first = first.0 - t0;
    let total: Vec<f64> = rows.iter().map(|r| r.2).collect();
    let y: Vec<f64> = rows.iter().map(|r| r.3).collect();
    let lock = rows.iter().position(|r| r.4);
    let lock_ms = lock.map(|l| to_first + rows[l].1.saturating_sub(first.1));
    format!(
        "{what} ({}): -> first frame {:.1} ms, first locked frame {} ({}), exposure x gain within 5% after {} frames, output level within 5% after {} frames (first {:.1}, final {:.1}); {:.0} us x {:.2} -> {:.0} us x {:.2}",
        if first.7 { "warm" } else { "cold" },
        ms(to_first),
        lock.map_or("never".into(), |l| l.to_string()),
        lock_ms.map_or("-".into(), |d| format!("{:.1} ms", ms(d))),
        settle_index(&total, 0.05).unwrap_or(0),
        settle_index(&y, 0.05).unwrap_or(0),
        y[0],
        y[y.len() - 1],
        ms(first.5) * 1e3,
        first.6,
        ms(last.5) * 1e3,
        last.6,
    )
}
