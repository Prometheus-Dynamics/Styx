//! Raw frames and your own processing, recorded and replayed.
//!
//! `record` captures raw Bayer frames (a native sensor's packed RAW10, or any camera with a
//! Bayer mode), writes them losslessly to an MCAP recording (`StreamRecorder`; opens in
//! Foxglove) and runs them through your own processing: the software ISP (`styx-softisp`)
//! with a white balance and brightness loop written here, from the statistics the ISP
//! gathers in the same pass. `replay` plays the recording back as a camera
//! (`CaptureRequest::replay_source`) and runs the same processing, anywhere: no camera, no
//! ISP hardware.
//!
//! ```sh
//! cargo run -p styx-examples --features native,replay-mcap --bin raw_processing -- record /tmp/raw.mcap 60 [exposure-us] [gain]
//! cargo run -p styx-examples --features native,replay-mcap --bin raw_processing -- replay /tmp/raw.mcap
//! ```
//!
//! The last processed frame is saved next to the recording (`.ppm`). MCAP recordings keep each
//! frame's pixels, timestamp and sequence, and for a sensor Styx drives the exposure, gains,
//! frame duration and frame length that produced it (and whether they were read back from the
//! frame): replayed frames report them like live ones.

use std::io::Write;
use std::time::{Duration, Instant};

use styx::capture_api::native_controls as ctl;
use styx::prelude::*;
use styx_softisp::{
    BlackLevel, IspParams, OutputBuffers, RawFormat, Scale, SoftIsp, StatsConfig, ToneCurve,
    WhiteBalance,
};

/// A grey-world white balance and a brightness target, updated from each frame's statistics.
struct MyProcessing {
    isp: Option<SoftIsp>,
    gains: [f32; 3],
    digital_gain: f32,
    rgb: Vec<u8>,
    took: Duration,
}

impl MyProcessing {
    fn new() -> Self {
        Self {
            isp: None,
            gains: [1.0; 3],
            digital_gain: 1.0,
            rgb: Vec::new(),
            took: Duration::ZERO,
        }
    }

    fn process(&mut self, frame: &FrameLease) -> Result<f32, Box<dyn std::error::Error>> {
        let t = Instant::now();
        let f = frame.meta().format;
        let (w, h) = (f.resolution.width.get(), f.resolution.height.get());
        let format = RawFormat::from_fourcc(f.code, w, h).ok_or("not a Bayer format")?;
        let params = IspParams {
            // 64 at 10 bits (the OV9782's), scaled to the format's depth.
            black_level: Some(BlackLevel::uniform(
                64u16 << format.packing.bit_depth().saturating_sub(10),
            )),
            white_balance: Some(WhiteBalance {
                r: self.gains[0],
                g: self.gains[1],
                b: self.gains[2],
            }),
            digital_gain: self.digital_gain,
            tone: Some(ToneCurve::Srgb),
            stats: Some(StatsConfig {
                row_step: 4,
                ..Default::default()
            }),
            ..Default::default()
        };
        let isp = match &mut self.isp {
            Some(isp) if isp.format() == format => {
                isp.set_params(params)?;
                isp
            }
            slot => slot.insert(SoftIsp::new(format, params)?.with_threads(2)),
        };
        self.rgb.resize(w as usize * h as usize * 3, 0);
        let planes = frame.planes();
        let stats = isp
            .process(
                planes[0].data(),
                planes[0].stride(),
                Scale::Full,
                OutputBuffers::Rgb24 {
                    data: &mut self.rgb,
                    stride: w as usize * 3,
                },
            )?
            .ok_or("no statistics")?;
        // Grey world: the mean colour of the unclipped zones, before this frame's gains.
        let mut sum = [0f64; 3];
        for zone in &stats.zones {
            if let Some(rgb) = zone.mean_rgb() {
                for c in 0..3 {
                    sum[c] += f64::from(rgb[c] / stats.gains[c]);
                }
            }
        }
        if sum.iter().all(|&s| s > 0.0) {
            let gain = |g: f64| (g as f32).clamp(0.25, 8.0);
            self.gains = [gain(sum[1] / sum[0]), 1.0, gain(sum[1] / sum[2])];
        }
        // Brightness: move the mean luma halfway (in stops) toward 0.2 each frame.
        let mean = stats.mean_luma().max(1e-3);
        self.digital_gain = (self.digital_gain * (0.2 / mean).sqrt()).clamp(1.0, 8.0);
        self.took += t.elapsed();
        Ok(mean)
    }

    fn save(&self, path: &str, w: u32, h: u32) -> std::io::Result<()> {
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        write!(out, "P6\n{w} {h}\n255\n")?;
        out.write_all(&self.rgb)?;
        out.flush()
    }
}

/// Runs `count` frames from `handle` through the processing (and the recorder, if any).
fn run(
    handle: &CaptureHandle,
    count: usize,
    mut recorder: Option<&mut StreamRecorder>,
    out: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut mine = MyProcessing::new();
    let mut n = 0;
    let mut size = (0, 0);
    while n < count {
        let frame = match handle.recv_blocking(Duration::from_secs(2)) {
            RecvOutcome::Data(frame) => frame,
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => break,
        };
        if let Some(recorder) = recorder.as_deref_mut() {
            recorder.record(&frame)?;
        }
        let mean = mine.process(&frame)?;
        n += 1;
        let f = frame.meta().format;
        size = (f.resolution.width.get(), f.resolution.height.get());
        if n % 15 == 1 {
            let made = frame.meta().native().map_or(String::new(), |m| {
                format!(
                    ", sensor {:.2} ms x {:.2}",
                    m.exposure_ns as f64 / 1e6,
                    m.gain()
                )
            });
            println!(
                "frame {:>4} {} {}x{}{made}: luma {mean:.3}, white balance R {:.2} B {:.2}, digital gain {:.2}",
                frame.meta().sequence().unwrap_or(0),
                f.code,
                size.0,
                size.1,
                mine.gains[0],
                mine.gains[2],
                mine.digital_gain
            );
        }
    }
    println!(
        "{n} frames, software ISP + statistics {:.2} ms per frame",
        mine.took.as_secs_f64() * 1e3 / n.max(1) as f64
    );
    mine.save(out, size.0, size.1)?;
    println!("saved {out}");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "/tmp/raw.mcap".into());
    let out = format!("{}.ppm", path.trim_end_matches(".mcap"));
    match args.first().map(String::as_str) {
        Some("record") => {
            let count: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(60);
            // The first camera with a raw Bayer mode, at 30 fps.
            let devices = styx::probe_all();
            let (device, backend, mode) = devices
                .iter()
                .flat_map(|d| d.backends.iter().map(move |b| (d, b)))
                .find_map(|(d, b)| {
                    let mode = b
                        .descriptor
                        .modes
                        .iter()
                        .find(|m| styx_softisp::bayer_fourcc(m.format.code).is_some())?;
                    Some((d, b.kind, mode.id.clone()))
                })
                .ok_or("no camera with a raw Bayer mode")?;
            println!(
                "{} via {backend}: {} {}x{}",
                device.identity.display,
                mode.format.code,
                mode.format.resolution.width,
                mode.format.resolution.height
            );
            let mut request = CaptureRequest::new(device)
                .backend(backend)
                .mode(mode)
                .interval(Interval::from_fps(30).ok_or("fps")?);
            if backend == BackendKind::Native {
                // Raw frames have no AE: a fixed exposure and gain (from the command line).
                let exposure_us: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(20_000);
                let gain: f32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(4.0);
                request = request
                    .control(ctl::EXPOSURE_TIME_US, ControlValue::Uint(exposure_us))
                    .control(ctl::GAIN, ControlValue::Float(gain));
            }
            let handle = request.start()?;
            let mut recorder = StreamRecorder::create(&path, device, &handle)?;
            run(&handle, count, Some(&mut recorder), &out)?;
            println!(
                "recorded {} frames to {}",
                recorder.frames(),
                recorder.finish()?.display()
            );
            handle.stop();
        }
        Some("replay") => {
            let source = CaptureRequest::replay_source(
                ReplaySourceConfig::new(&path).pacing(ReplayPacing::Unpaced),
            )?;
            println!("replaying {}", source.device().identity.display);
            let handle = source.open()?;
            run(&handle, usize::MAX, None, &out)?;
            handle.stop();
        }
        _ => println!("usage: raw_processing record OUT.mcap [frames] | replay IN.mcap"),
    }
    Ok(())
}
