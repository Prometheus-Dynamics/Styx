//! `replay`: the software ISP loop over a raw recording, with a virtual sensor that re-exposes
//! the recorded frames for whatever the loop asks (see `styx_pipeline::replay`).

use std::time::{Duration, Instant};

use styx_pipeline::measure::{grey_ratios, write_ppm};
use styx_pipeline::rawrec::RawRecording;
use styx_pipeline::replay::VirtualSensor;
use styx_pipeline::{SensorInfo, SoftLoop};
use styx_softisp::{OutputBuffers, Scale};

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
    let mut rgb = vec![0u8; w * ht * 3];
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
            .process(
                &raw,
                stride,
                &values,
                Scale::Full,
                OutputBuffers::Rgb24 {
                    data: &mut rgb,
                    stride: w * 3,
                },
            )
            .map_err(|e| e.to_string())?;
        let processing = t.elapsed();
        if let Some(r) = &out.step.sensor {
            sensor.request(r);
        }
        if controls_for(a, i, Some((values.exposure, values.analogue_gain))).is_none() {
            base_values = Some((values.exposure, values.analogue_gain));
        }
        let mut log = FrameLog::new(&values, &out.step, frame_period * i as u32);
        log.processing = processing;
        log.request_lands = out.step.sensor.map(|r| r.frame);
        log.out_y = luma_of_rgb(&rgb, w, ht);
        if !a.quiet {
            println!("{}", log.line());
        }
        frames.push(log);
    }
    soft.controller()
        .stop_recording()
        .map_err(|e| e.to_string())?;
    let wall = t0.elapsed();
    let ppm = a.out.join("replay-soft-rgb.ppm");
    write_ppm(&ppm, &rgb, w, ht, w * 3).map_err(|e| e.to_string())?;
    write_csv(&a.out.join("replay-frames.csv"), &frames).map_err(|e| e.to_string())?;
    let (rg, bg) = grey_ratios(&rgb, w, ht, w * 3, 16, 240);
    let summary = Summary {
        name: "replay (software ISP, virtual sensor)",
        frames: &frames,
        open_to_first: Duration::ZERO,
        cpu: frames.iter().map(|f| f.processing).sum(),
        wall,
        peak_rss: 0,
        extra: vec![
            format!("output grey-world ratios R/G {rg:.3} B/G {bg:.3} (1.000 is neutral)"),
            format!("saved {}", ppm.display()),
        ],
    };
    print!("{}", summary.render(a));
    Ok(())
}

/// Mean BT.601 luma of an RGB24 image, 0..255.
pub fn luma_of_rgb(rgb: &[u8], w: usize, h: usize) -> f64 {
    let m = styx_pipeline::measure::rgb_means(rgb, w, h, w * 3);
    0.299 * m[0] + 0.587 * m[1] + 0.114 * m[2]
}
