//! PiSP spike: the front end configured from Rust with statistics decoded per frame, then a
//! captured raw frame through the back end to NV12 and RGB.
//!
//! Needs the normal kernel-driver path: the sensor driver (`ov9282`, OV9782 variant) bound,
//! nothing else using `rp1-cfe` (stop `helios-peripherals`), under the device lock.
//!
//! ```text
//! pisp-spike [--frames N] [--out DIR] [--be-runs N] [--no-be]
//! ```

use std::process::ExitCode;
use std::time::{Duration, Instant};

use styx_kernel::subdev::MbusCode;
use styx_pisp::be::BackEnd;
use styx_pisp::device::{BackEndDevice, BeOutput, FrontEndDevice, FrontEndSetup};
use styx_pisp::fe::FrontEnd;
use styx_pisp::stats::Statistics;
use styx_pisp::uapi::{BayerOrder, BeOutputFormatConfig, ImageFormatConfig, fe_enable};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;
/// OV9782 black level on the 16-bit scale (libcamera's ov9782.json).
const BLACK: u16 = 4096;
const TIMEOUT: Duration = Duration::from_secs(2);

struct Args {
    frames: usize,
    out: String,
    be_runs: usize,
    be: bool,
}

fn args() -> Result<Args, String> {
    let mut a = Args {
        frames: 30,
        out: "/tmp/styx-pisp".into(),
        be_runs: 20,
        be: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        let mut val = || it.next().ok_or(format!("{x} needs a value"));
        match x.as_str() {
            "--frames" => a.frames = val()?.parse().map_err(|e| format!("--frames: {e}"))?,
            "--be-runs" => a.be_runs = val()?.parse().map_err(|e| format!("--be-runs: {e}"))?,
            "--out" => a.out = val()?,
            "--no-be" => a.be = false,
            _ => return Err(format!("unknown argument {x}")),
        }
    }
    Ok(a)
}

/// Mean R, G, B of a 16-bit BGGR frame after black level subtraction.
fn raw_means(raw: &[u8], stride: usize) -> (f64, f64, f64) {
    let px = |x: usize, y: usize| {
        let o = y * stride + 2 * x;
        f64::from(u16::from_le_bytes([raw[o], raw[o + 1]]).saturating_sub(BLACK))
    };
    let (mut r, mut g, mut b) = (0.0, 0.0, 0.0);
    let mut n = 0.0;
    for y in (0..HEIGHT as usize).step_by(2) {
        for x in (0..WIDTH as usize).step_by(2) {
            b += px(x, y);
            g += px(x + 1, y) + px(x, y + 1);
            r += px(x + 1, y + 1);
            n += 1.0;
        }
    }
    (r / n, g / (2.0 * n), b / n)
}

fn summarise(seq: u32, dt: Option<Duration>, s: &Statistics) -> String {
    let t = s.awb_total();
    let mean = t.mean().unwrap_or_default();
    let centre = s.awb_zones[16 * 32 + 16].mean().unwrap_or_default();
    format!(
        "seq {seq:4} dt {:6.2} ms | AWB zones counted {} mean R {:.1} G {:.1} B {:.1} (centre {:.1}/{:.1}/{:.1}) | \
         hist n {} mean {:.1} p5 {:?} p50 {:?} p95 {:?} | AGC float0 {:.1} | rows>0 {} | focus sum {}",
        dt.map_or(0.0, |d| d.as_secs_f64() * 1e3),
        t.counted,
        mean.0,
        mean.1,
        mean.2,
        centre.0,
        centre.1,
        centre.2,
        s.histogram_count(),
        s.histogram_mean().unwrap_or(0.0),
        s.histogram_quantile(0.05),
        s.histogram_quantile(0.5),
        s.histogram_quantile(0.95),
        s.agc_floating[0].mean().unwrap_or(0.0),
        s.row_sums.iter().filter(|&&r| r > 0).count(),
        s.focus.iter().sum::<u64>(),
    )
}

fn front_end(a: &Args) -> Result<(Vec<u8>, ImageFormatConfig, Statistics), String> {
    let setup = FrontEndSetup {
        width: WIDTH,
        height: HEIGHT,
        sensor_code: MbusCode::SBGGR10_1X10,
        bayer: BayerOrder::Bggr,
        image_output: true,
        buffers: 4,
    };
    let mut dev = FrontEndDevice::open(&setup).map_err(|e| format!("front end open: {e}"))?;
    let img = dev.image_format();
    println!(
        "[fe] graph set up; raw output {}x{} stride {}",
        img.width, img.height, img.stride
    );
    let mut fe = FrontEnd::new(WIDTH as u16, HEIGHT as u16, BayerOrder::Bggr);
    fe.default_stats(BLACK, 1.5, 1.5);
    fe.set_output_format(0, img);
    fe.enable(fe_enable::OUTPUT0, true);
    let t0 = Instant::now();
    dev.start(&mut fe, 2)
        .map_err(|e| format!("front end start: {e}"))?;
    let mut last: Option<(Vec<u8>, Statistics)> = None;
    let mut prev_ts = None;
    let mut mismatched = 0;
    let mut result = Ok(());
    for i in 0..a.frames {
        match dev.next_frame(&mut fe, TIMEOUT) {
            Ok(f) => {
                if i == 0 {
                    println!(
                        "[fe] first frame after {:.1} ms",
                        t0.elapsed().as_secs_f64() * 1e3
                    );
                }
                let dt = prev_ts.map(|p| f.timestamp.saturating_sub(p));
                prev_ts = Some(f.timestamp);
                let raw = f.raw.expect("image output enabled");
                if raw.0 != f.sequence {
                    mismatched += 1;
                }
                println!("[fe] {}", summarise(f.sequence, dt, &f.stats));
                last = Some((raw.1, f.stats));
            }
            Err(e) => {
                result = Err(format!("frame {i}: {e}"));
                break;
            }
        }
    }
    let stop = dev.stop();
    result?;
    stop.map_err(|e| format!("front end stop: {e}"))?;
    let (raw, stats) = last.ok_or("no frames")?;
    let (r, g, b) = raw_means(&raw, img.stride as usize);
    let t = stats.awb_total().mean().unwrap_or_default();
    println!(
        "[fe] {} frames, {} raw/stats sequence mismatches; last raw frame mean minus black R {r:.1} G {g:.1} B {b:.1}; AWB mean R {:.1} G {:.1} B {:.1}",
        a.frames, mismatched, t.0, t.1, t.2
    );
    let path = format!("{}/fe-raw-{WIDTH}x{HEIGHT}-BYR2.raw", a.out);
    std::fs::write(&path, &raw).map_err(|e| format!("{path}: {e}"))?;
    println!("[fe] saved {path}");
    Ok((raw, img, stats))
}

fn back_end(
    a: &Args,
    raw: &[u8],
    input: ImageFormatConfig,
    stats: &Statistics,
    out: BeOutput,
) -> Result<(), String> {
    let t = stats.awb_total().mean().unwrap_or((1.0, 1.0, 1.0));
    let (gr, gb) = if t.0 > 0.0 && t.2 > 0.0 {
        ((t.1 / t.0).clamp(0.5, 8.0), (t.1 / t.2).clamp(0.5, 8.0))
    } else {
        (1.0, 1.0)
    };
    let mut dev = BackEndDevice::open(0, input, BayerOrder::Bggr, out)
        .map_err(|e| format!("back end open: {e}"))?;
    let of = dev.output_format();
    let mut be = BackEnd::simple_bayer(
        input,
        BayerOrder::Bggr,
        BLACK,
        (gr, 1.0, gb),
        None,
        out.pisp_format(),
    );
    be.set_output_format(
        0,
        BeOutputFormatConfig {
            image: of,
            ..Default::default()
        },
    );
    let t0 = Instant::now();
    let cfg = be.prepare().map_err(|e| e.to_string())?;
    let prep = t0.elapsed();
    println!(
        "[be] {out:?}: grey-world gains R {gr:.2} B {gb:.2}; config prepared in {:.1} us, {} tiles; output stride {}",
        prep.as_secs_f64() * 1e6,
        cfg.num_tiles,
        of.stride
    );
    let mut times = Vec::new();
    let mut output = Vec::new();
    let mut result = Ok(());
    for _ in 0..a.be_runs.max(1) {
        match dev.process(raw, &cfg, TIMEOUT) {
            Ok((o, d)) => {
                times.push(d);
                output = o;
            }
            Err(e) => {
                result = Err(format!("back end job: {e}"));
                break;
            }
        }
    }
    let stop = dev.stop();
    result?;
    stop.map_err(|e| format!("back end stop: {e}"))?;
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    println!(
        "[be] {} jobs: min {:.2} ms median {:.2} ms max {:.2} ms (queue config -> output dequeued)",
        times.len(),
        ms(times[0]),
        ms(times[times.len() / 2]),
        ms(times[times.len() - 1])
    );
    let (w, h, s) = (of.width as usize, of.height as usize, of.stride as usize);
    let (name, data) = match out {
        BeOutput::Nv12 => {
            let luma: f64 = (0..h)
                .map(|y| {
                    output[y * s..y * s + w]
                        .iter()
                        .map(|&v| f64::from(v))
                        .sum::<f64>()
                })
                .sum::<f64>()
                / (w * h) as f64;
            println!("[be] NV12 {} bytes, mean Y {luma:.1}", output.len());
            let mut pgm = format!("P5\n{w} {h}\n255\n").into_bytes();
            for y in 0..h {
                pgm.extend_from_slice(&output[y * s..y * s + w]);
            }
            std::fs::write(format!("{}/be-nv12.raw", a.out), &output).map_err(|e| e.to_string())?;
            ("be-nv12-luma.pgm", pgm)
        }
        BeOutput::Rgb24 => {
            let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
            for y in 0..h {
                ppm.extend_from_slice(&output[y * s..y * s + 3 * w]);
            }
            let n = (w * h) as f64;
            let mean = |c: usize| {
                (0..w * h)
                    .map(|i| f64::from(ppm[ppm.len() - 3 * w * h + 3 * i + c]))
                    .sum::<f64>()
                    / n
            };
            println!(
                "[be] RGB24 mean R {:.1} G {:.1} B {:.1}",
                mean(0),
                mean(1),
                mean(2)
            );
            ("be-rgb.ppm", ppm)
        }
    };
    let path = format!("{}/{name}", a.out);
    std::fs::write(&path, data).map_err(|e| format!("{path}: {e}"))?;
    println!("[be] saved {path}");
    Ok(())
}

fn run() -> Result<(), String> {
    let a = args()?;
    std::fs::create_dir_all(&a.out).map_err(|e| format!("{}: {e}", a.out))?;
    let (raw, img, stats) = front_end(&a)?;
    if a.be {
        back_end(&a, &raw, img, &stats, BeOutput::Nv12)?;
        back_end(&a, &raw, img, &stats, BeOutput::Rgb24)?;
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pisp-spike: {e}");
            ExitCode::FAILURE
        }
    }
}
