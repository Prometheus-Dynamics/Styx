//! Still capture while the preview keeps running: a JPEG plus a DNG, then a three-shot
//! exposure bracket (-1, 0, +1 EV, each on the frame the control schedule says its exposure
//! lands on), with the preview's rate and sequence gaps measured meanwhile.
//!
//! ```sh
//! cargo run -p styx-examples --features native,v4l2,image --bin still_capture -- [out-dir] [fps]
//! ```
//!
//! On a native camera with an ISP (the PiSP or the software ISP) the preview is the processed
//! NV12 stream and stills come from its raw frames, reprocessed at full quality on another
//! thread (the PiSP back end's second node group, or the software ISP's best demosaic); the
//! DNG carries the tuning's colour calibration, black and white levels, lens shading and the
//! exposure. Other cameras (V4L2) give their next frame as the still, without DNG or bracket.

use std::time::{Duration, Instant};

use styx::prelude::*;

/// What the preview consumer saw.
#[derive(Default)]
struct Preview {
    frames: u64,
    gaps: u64,
    first_ts: u64,
    last_ts: u64,
    max_interval_ns: u64,
    last_seq: Option<u32>,
}

fn preview_loop(handle: &CaptureHandle, stop: &std::sync::atomic::AtomicBool) -> Preview {
    let mut p = Preview::default();
    while !stop.load(std::sync::atomic::Ordering::Acquire) {
        let RecvOutcome::Data(frame) = handle.recv_blocking(Duration::from_millis(200)) else {
            continue;
        };
        let m = frame.meta();
        let ts = m.timestamp;
        if p.frames == 0 {
            p.first_ts = ts;
        } else {
            p.max_interval_ns = p.max_interval_ns.max(ts.saturating_sub(p.last_ts));
        }
        if let (Some(last), Some(seq)) = (p.last_seq, m.sequence()) {
            p.gaps += u64::from(seq.saturating_sub(last).saturating_sub(1));
        }
        p.last_seq = m.sequence();
        p.last_ts = ts;
        p.frames += 1;
    }
    p
}

fn describe(shot: &StillShot) -> String {
    let m = &shot.meta;
    format!(
        "frame {} (target {}, landed {}), {:.2} ms x {:.2} (ISP x {:.2}), {:.0} K, {:.0} lux, \
         EV {:+.1}, {} in {:.1} ms",
        m.sequence,
        m.target_frame.map_or("-".to_string(), |t| t.to_string()),
        m.landed,
        m.exposure.as_secs_f64() * 1e3,
        m.analogue_gain,
        m.isp_digital_gain,
        m.colour_temperature,
        m.lux,
        m.ev,
        m.isp,
        m.process_time.as_secs_f64() * 1e3
    )
}

fn save(shot: &StillShot, dir: &std::path::Path, name: &str) -> std::io::Result<()> {
    if let Some(img) = &shot.image {
        let ext = match img.format {
            FourCc::MJPG => "jpg",
            FourCc::NV12 => "nv12",
            FourCc::RG24 => "rgb",
            _ => "raw",
        };
        shot.save_image(dir.join(format!("{name}.{ext}")))?;
    }
    if shot.dng.is_some() {
        shot.save_dng(dir.join(format!("{name}.dng")))?;
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = std::path::PathBuf::from(args.first().map_or("/tmp/stills", String::as_str));
    let fps: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    std::fs::create_dir_all(&dir)?;
    let devices = styx::probe_all();
    // A native camera's processed NV12 mode, else any camera's first mode.
    let (device, backend, mode) = devices
        .iter()
        .find_map(|d| {
            let b = d.backend(BackendKind::Native)?;
            let m = b
                .descriptor
                .modes
                .iter()
                .find(|m| m.format.code == FourCc::NV12)?;
            Some((d, b.kind, m.id.clone()))
        })
        .or_else(|| {
            devices.iter().find_map(|d| {
                let b = d.backends.first()?;
                Some((d, b.kind, b.descriptor.modes.first()?.id.clone()))
            })
        })
        .ok_or("no camera")?;
    let native = backend == BackendKind::Native;
    println!(
        "{} via {backend}: {} {}x{} at {fps} fps",
        device.identity.display,
        mode.format.code,
        mode.format.resolution.width,
        mode.format.resolution.height
    );
    let handle = CaptureRequest::new(device)
        .backend(backend)
        .mode(mode)
        .interval(Interval::from_fps(fps).ok_or("fps")?)
        .start()?;
    let stop = std::sync::atomic::AtomicBool::new(false);
    let preview = std::thread::scope(|s| -> Result<Preview, Box<dyn std::error::Error>> {
        let consumer = s.spawn(|| preview_loop(&handle, &stop));
        let stills = take_stills(&handle, &dir, native);
        stop.store(true, std::sync::atomic::Ordering::Release);
        let preview = consumer.join().map_err(|_| "preview thread panicked")?;
        stills.map(|()| preview)
    })?;
    handle.stop();
    let secs = (preview.last_ts.saturating_sub(preview.first_ts)) as f64 / 1e9;
    println!(
        "preview: {} frames in {secs:.2} s ({:.2} fps), {} sequence gaps, longest interval {:.1} ms",
        preview.frames,
        preview.frames.saturating_sub(1) as f64 / secs.max(1e-9),
        preview.gaps,
        preview.max_interval_ns as f64 / 1e6
    );
    println!("files in {}", dir.display());
    Ok(())
}

/// A still (JPEG, and a DNG on native cameras), then a bracket, while the preview runs.
fn take_stills(
    handle: &CaptureHandle,
    dir: &std::path::Path,
    native: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Let AE and AWB settle on the preview.
    std::thread::sleep(Duration::from_secs(2));

    let t = Instant::now();
    let still = handle.capture_still(&StillRequest::jpeg(92).with_dng(native).settle(true))?;
    save(&still.shots[0], dir, "still")?;
    println!(
        "still: {} (request -> files {:.1} ms)\n  {}",
        dir.join("still.jpg").display(),
        t.elapsed().as_secs_f64() * 1e3,
        describe(&still.shots[0])
    );
    std::thread::sleep(Duration::from_millis(500));

    if native {
        let t = Instant::now();
        let bracket = handle.capture_still(
            &StillRequest::jpeg(92)
                .with_dng(true)
                .bracket([-1.0, 0.0, 1.0]),
        )?;
        for (i, shot) in bracket.shots.iter().enumerate() {
            save(shot, dir, &format!("bracket{i}"))?;
            println!("bracket {i}: {}", describe(shot));
        }
        println!(
            "bracket: {} shots, request -> files {:.1} ms",
            bracket.shots.len(),
            t.elapsed().as_secs_f64() * 1e3
        );
    }
    std::thread::sleep(Duration::from_secs(1));
    Ok(())
}
