//! `replay`: the software ISP loop over a raw recording, with a virtual sensor that re-exposes
//! the recorded frames for whatever the loop asks (see `styx_pipeline::replay`).

use std::time::{Duration, Instant};

use styx_pipeline::rawrec::RawRecording;
use styx_pipeline::replay::VirtualSensor;
use styx_pipeline::{SensorInfo, SoftLoop};

use crate::report::{FrameLog, Summary, write_csv};
use crate::{Args, controls_for, tuning};

pub fn run(a: &Args) -> Result<(), String> {
    let base = a
        .recording
        .as_ref()
        .ok_or("replay needs --recording BASE")?;
    let rec = RawRecording::open(base).map_err(|e| format!("{}: {e}", base.display()))?;
    if rec.is_empty() {
        return Err("the recording has no frames".into());
    }
    let h = &rec.header;
    println!(
        "recording: {} frames {}x{} {:?} stride {} ({})",
        rec.len(),
        h.format.width,
        h.format.height,
        h.format.packing,
        h.stride,
        h.notes
    );
    let info: SensorInfo = h
        .sensor
        .clone()
        .with_fps(a.fps, a.fps)
        .map_err(|e| e.to_string())?;
    let mut sensor = VirtualSensor::new(&rec);
    let format = sensor.format();
    let mut soft =
        SoftLoop::new(info, format.packing, &tuning(a)?, a.threads).map_err(|e| e.to_string())?;
    soft.set_base_params(crate::soft_base(a));
    if a.every_frame {
        soft.set_settled_rate(None);
    }
    if let Some(p) = &a.algo_record {
        let f = std::fs::File::create(p).map_err(|e| format!("{}: {e}", p.display()))?;
        soft.controller()
            .record_to(std::io::BufWriter::new(f))
            .map_err(|e| e.to_string())?;
    }
    let start = soft.start().map_err(|e| e.to_string())?;
    if let Some(r) = start.sensor {
        sensor.request(&r);
    }
    let (w, ht) = (format.width as usize, format.height as usize);
    let mut output = crate::output::Output::new(a.output.0, a.output.1, w, ht);
    let mut frames = Vec::new();
    let mut base_values = None;
    let t0 = Instant::now();
    let frame_period = Duration::from_secs_f64(1.0 / a.fps);
    for i in 0..a.frames as u64 {
        if crate::interrupted() {
            break;
        }
        match controls_for(a, i, base_values) {
            Some(c) => soft.controller().set_controls(c),
            None => soft.controller().set_controls(Default::default()),
        }
        let stride = sensor.stride();
        let (raw, values) = sensor.next_frame();
        let raw = raw.to_vec();
        let t = Instant::now();
        let out = soft
            .process(&raw, stride, &values, output.scale, output.buffers())
            .map_err(|e| e.to_string())?;
        let processing = t.elapsed();
        if let Some(r) = &out.step.sensor {
            sensor.request(r);
        }
        if controls_for(a, i, Some((values.exposure, values.analogue_gain)))
            .is_none_or(|c| c.ae_enable)
        {
            base_values = Some((values.exposure, values.analogue_gain));
        }
        let mut log = FrameLog::new(&values, &out.step, frame_period * i as u32);
        (log.isp_dg, log.deflicker) = (out.applied.digital_gain, out.applied.flicker);
        log.processing = processing;
        log.request_lands = out.step.sensor.map(|r| r.frame);
        log.out_y = output.level();
        log.timing = Some(out.timing);
        if !a.quiet {
            println!("{}", log.line());
        }
        frames.push(log);
    }
    soft.controller()
        .stop_recording()
        .map_err(|e| e.to_string())?;
    let wall = t0.elapsed();
    let (saved, ratios) = output
        .save(&a.out, "replay-soft")
        .map_err(|e| e.to_string())?;
    write_csv(&a.out.join("replay-frames.csv"), &frames).map_err(|e| e.to_string())?;
    let summary = Summary {
        name: "replay (software ISP, virtual sensor)",
        frames: &frames,
        open_to_first: Duration::ZERO,
        cpu: frames.iter().map(|f| f.processing).sum(),
        wall,
        peak_rss: 0,
        extra: crate::output::summary_lines(&saved, ratios),
    };
    print!("{}", summary.render(a));
    Ok(())
}
