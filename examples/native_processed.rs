//! Processed frames from a native camera through the normal Styx API: the planner picks the
//! native camera's `NV12` / `RG24` modes (PiSP on a Raspberry Pi 5 / CM5, the software ISP
//! elsewhere), the 3A loop runs inside the capture, and frames carry the exposure and gain
//! that produced them. Prints each plan, the measured rate, how the exposure settled, CPU and
//! memory, and saves the last frame of each.
//!
//! ```sh
//! STYX_SENSOR_PATH=/tmp/styx-np/ov9782.toml native_processed [frames] [out-dir]
//! ```

use std::io::Write;
use std::time::{Duration, Instant};

use styx::prelude::*;

fn cpu_seconds() -> f64 {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            let rest = s.rsplit_once(')')?.1.split_whitespace().collect::<Vec<_>>();
            let u: f64 = rest.get(11)?.parse().ok()?;
            let k: f64 = rest.get(12)?.parse().ok()?;
            Some((u + k) / 100.0)
        })
        .unwrap_or(0.0)
}

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn save(path: &str, f: &FrameLease) -> std::io::Result<()> {
    let fmt = f.meta().format;
    let (w, h) = (
        fmt.resolution.width.get() as usize,
        fmt.resolution.height.get() as usize,
    );
    let planes = f.planes();
    let p = &planes[0];
    let (magic, bpp) = if matches!(fmt.code, FourCc::NV12 | FourCc::GREY | FourCc::R8) {
        ("P5", 1)
    } else {
        ("P6", 3)
    };
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(out, "{magic}\n{w} {h}\n255\n")?;
    for y in 0..h {
        out.write_all(&p.data()[y * p.stride()..y * p.stride() + w * bpp])?;
    }
    out.flush()
}

fn main() -> Result<(), CaptureError> {
    let args: Vec<String> = std::env::args().collect();
    let frames: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(90);
    let out = args.get(2).cloned().unwrap_or_else(|| "/tmp".into());
    let probe = styx::probe_all_with_errors_with_config(&StyxConfig::default());
    for e in &probe.errors {
        println!("probe error: {e}");
    }
    for d in &probe.devices {
        for b in &d.backends {
            println!(
                "{} via {}: {} modes, isp {:?}",
                d.identity.display,
                b.kind,
                b.descriptor.modes.len(),
                b.properties
                    .iter()
                    .find(|(k, _)| k == "isp")
                    .map(|(_, v)| v)
            );
        }
    }
    let wants = [
        ("nv12", FrameRequirements::formats([FourCc::NV12])),
        ("luma", FrameRequirements::luma()),
        ("rgb", FrameRequirements::formats([FourCc::RG24])),
    ];
    for (name, req) in wants {
        let req = req.min_fps(30).max_resolution(1280, 800);
        let plan = match styx::planner::plan_best(&probe.devices, &req) {
            Ok(p) => p,
            Err(e) => {
                println!("\n{name}: {e}");
                continue;
            }
        };
        print!("\n{plan}");
        let (cpu0, t0) = (cpu_seconds(), Instant::now());
        let mut planned = plan.start()?;
        let mut first = None;
        let mut exposures = Vec::new();
        let mut last = None;
        let (mut seq0, mut ts0, mut seq1, mut ts1) = (0u32, 0u64, 0u32, 0u64);
        for i in 0..frames {
            let RecvOutcome::Data(f) = planned.next_frame(Duration::from_secs(3)) else {
                println!("  no frame after {i}");
                break;
            };
            first.get_or_insert(t0.elapsed());
            let seq = f.meta().sequence().unwrap_or(0);
            if i == 1 {
                (seq0, ts0) = (seq, f.meta().timestamp);
            }
            (seq1, ts1) = (seq, f.meta().timestamp);
            if let Some(BackendFrameMeta::Native(n)) = &f.meta().backend {
                exposures.push(n.exposure_ns as f64 * f64::from(n.analog_gain) / 1e3);
            }
            last = Some(f);
        }
        let wall = t0.elapsed().as_secs_f64();
        let cpu = cpu_seconds() - cpu0;
        let fps = f64::from(seq1.wrapping_sub(seq0)) / ((ts1.saturating_sub(ts0)) as f64 / 1e9);
        let settle = exposures.last().and_then(|&end| {
            exposures
                .iter()
                .rposition(|&v| (v - end).abs() > 0.05 * end)
                .map(|i| i + 1)
                .or(Some(0))
        });
        println!(
            "{name}: start -> first frame {:.1} ms, {fps:.3} fps, exposure x gain settled (5%) after {settle:?} frames at {:.0} us, CPU {:.1}% of one core, peak RSS {} KiB",
            first.unwrap_or_default().as_secs_f64() * 1e3,
            exposures.last().copied().unwrap_or(0.0),
            100.0 * cpu / wall.max(1e-9),
            rss_kib()
        );
        if let Some(f) = last {
            let ext = if f.meta().format.code != FourCc::RG24 {
                "pgm"
            } else {
                "ppm"
            };
            let path = format!("{out}/native-processed-{name}.{ext}");
            match save(&path, &f) {
                Ok(()) => println!("  saved {path}"),
                Err(e) => println!("  {path}: {e}"),
            }
        }
        planned.stop();
    }
    Ok(())
}
