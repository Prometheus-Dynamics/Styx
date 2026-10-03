//! Processed frames at a chosen size and frame rate, from whichever camera and route the
//! planner finds best: say what you want (`Frames::nv12().size(w, h).fps(n)`), open it, print
//! the plan it chose (camera, backend, mode, every step with where it runs and its estimated
//! cost, and the options it rejected), and look at the frames. On the CM5's OV9782 the plan is
//! the native backend with the PiSP and 3A in Rust; on a USB camera it is V4L2 plus a decoder
//! or converter; the code does not change.
//!
//! ```sh
//! cargo run -p styx-examples --features native,v4l2 --bin capture_frames -- nv12 1280x800 30 90
//! capture_frames rgb 640x400 60 120      # RGB24 at 640x400 (the PiSP's second output scales)
//! capture_frames gray 1280x800 30 90     # 8-bit grey (NV12's Y plane, no copy)
//! capture_frames nv12 1280x800 30 90 /tmp/out.pgm   # also save the last frame
//! ```
//!
//! `STYX_CAMERA=uvc` limits the choice to cameras whose name or keys contain that text.
//!
//! `.fps(n)` is exactly `n` frames per second: any rate within the range of a sensor Styx
//! drives, a listed rate on a USB camera (planning fails, naming the rates there are, when no
//! mode has it). `.fps_at_least(n)` takes the fastest rate instead; without either a camera runs
//! at its default (30 fps, or the listed rate closest to it). A rate of 0 here leaves it to the
//! camera.

use std::io::Write;
use std::time::{Duration, Instant};

use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize, default: &str| args.get(i).cloned().unwrap_or_else(|| default.into());
    let (w, h) = arg(1, "1280x800")
        .split_once('x')
        .map(|(w, h)| (w.parse(), h.parse()))
        .and_then(|(w, h)| Some((w.ok()?, h.ok()?)))
        .ok_or("size is WxH")?;
    let fps: u32 = arg(2, "30").parse()?;
    let count: usize = arg(3, "90").parse()?;

    let mut wants = match arg(0, "nv12").as_str() {
        "rgb" => Frames::rgb(),
        "luma" | "gray" => Frames::gray(),
        _ => Frames::nv12(),
    }
    .size(w, h);
    if fps > 0 {
        wants = wants.fps(fps);
    }

    let mut cameras = styx::probe_all();
    if let Ok(name) = std::env::var("STYX_CAMERA") {
        let name = name.to_lowercase();
        cameras.retain(|c| {
            std::iter::once(&c.identity.display)
                .chain(&c.identity.keys)
                .any(|k| k.to_lowercase().contains(&name))
        });
    }
    let opened = Instant::now();
    let mut frames = wants.open_best(&cameras)?;
    print!("{}", frames.plan());
    let (ow, oh) = frames.plan().output_resolution();
    println!("delivers {ow}x{oh}");

    let (mut first, mut last, mut n) = (None, None, 0usize);
    let mut seq = (None, 0u32);
    while n < count {
        let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_secs(2)) else {
            println!("no frame within 2 s");
            break;
        };
        n += 1;
        first.get_or_insert_with(|| opened.elapsed());
        let meta = frame.meta();
        let s = meta.sequence().unwrap_or(0);
        seq = (seq.0.or(Some((s, meta.timestamp))), s);
        if n <= 3 || n == count {
            // What produced the frame (native cameras): exposure, gains, frame duration.
            let made = meta.native().map_or(String::new(), |m| {
                format!(
                    ", exposure {:.2} ms x gain {:.2}",
                    m.exposure_ns as f64 / 1e6,
                    m.gain()
                )
            });
            println!(
                "frame {s}: {} {}x{}, {} plane(s), t={:.3} s{made}",
                meta.format.code,
                meta.format.resolution.width,
                meta.format.resolution.height,
                frame.planes().len(),
                meta.timestamp as f64 / 1e9,
            );
        }
        let timestamp = meta.timestamp;
        last = Some((frame, timestamp));
    }
    if let (Some((s0, t0)), Some((_, t1))) = (seq.0, &last) {
        let frames_between = f64::from(seq.1.wrapping_sub(s0));
        println!(
            "{n} frames, open -> first frame {:.1} ms, {:.3} fps by sensor timestamps",
            first.unwrap_or_default().as_secs_f64() * 1e3,
            frames_between / ((t1 - t0) as f64 / 1e9)
        );
    }
    if let (Some(path), Some((frame, _))) = (args.get(4), &last) {
        save(path, frame)?;
        println!("saved {path}");
    }
    frames.stop();
    Ok(())
}

/// Writes the frame's first plane as PGM (luma, NV12's Y) or PPM (RGB24).
fn save(path: &str, frame: &FrameLease) -> std::io::Result<()> {
    let format = frame.meta().format;
    let (w, h) = (
        format.resolution.width.get() as usize,
        format.resolution.height.get() as usize,
    );
    let (magic, bpp) = if format.code == FourCc::RG24 {
        ("P6", 3)
    } else {
        ("P5", 1)
    };
    let planes = frame.planes();
    let plane = &planes[0];
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(out, "{magic}\n{w} {h}\n255\n")?;
    for row in plane.data().chunks(plane.stride()).take(h) {
        out.write_all(&row[..w * bpp])?;
    }
    out.flush()
}
