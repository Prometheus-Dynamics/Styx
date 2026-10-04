//! Still capture while the preview keeps running: a JPEG plus a DNG, then a three-shot
//! exposure bracket (-1, 0, +1 EV, each on the frame the control schedule says its exposure
//! lands on), with the preview's rate and sequence gaps measured meanwhile.
//!
//! ```sh
//! cargo run -p styx-examples --features native,v4l2,image --bin still_capture -- [out-dir] [fps] [native|v4l2]
//! ```
//!
//! The preview is `Frames::nv12().fps(30).open(&camera)`; stills are asked for with
//! `Frames::request_still` (or `capture_still`, which waits) while this thread keeps taking
//! preview frames. On a native camera with an ISP (the PiSP or the software ISP) stills come
//! from the stream's raw frames, reprocessed at full quality on another thread (the PiSP back
//! end's second node group, or the software ISP's best demosaic); the DNG carries the
//! tuning's colour calibration, black and white levels, lens shading and the exposure. Other
//! cameras (V4L2) give their next frame as the still, without DNG or bracket.

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

impl Preview {
    fn add(&mut self, frame: &FrameLease) {
        let m = frame.meta();
        let ts = m.timestamp;
        if self.frames == 0 {
            self.first_ts = ts;
        } else {
            self.max_interval_ns = self.max_interval_ns.max(ts.saturating_sub(self.last_ts));
        }
        if let (Some(last), Some(seq)) = (self.last_seq, m.sequence()) {
            self.gaps += u64::from(seq.saturating_sub(last).saturating_sub(1));
        }
        self.last_seq = m.sequence();
        self.last_ts = ts;
        self.frames += 1;
    }
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

/// Takes preview frames for `secs`, or until `pending` is ready.
fn preview_for(
    frames: &mut Frames,
    stats: &mut Preview,
    secs: f64,
    mut pending: Option<&mut PendingStill>,
) -> Option<Result<StillCapture, CaptureError>> {
    let until = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < until {
        if let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_millis(200)) {
            stats.add(&frame);
        }
        if let Some(r) = pending.as_mut().and_then(|p| p.try_take()) {
            return Some(r);
        }
    }
    None
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = std::path::PathBuf::from(args.first().map_or("/tmp/stills", String::as_str));
    let fps: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    std::fs::create_dir_all(&dir)?;
    let kind = match args.get(2).map(String::as_str) {
        Some("v4l2") => BackendKind::V4l2,
        _ => BackendKind::Native,
    };
    let devices = styx::probe_all();
    let device = devices
        .iter()
        .find(|d| d.backend(kind).is_some())
        .or_else(|| devices.first())
        .ok_or("no camera")?;
    let mut request = Frames::nv12().fps(fps);
    if device.backend(kind).is_some() {
        request = request.backend(kind);
    }
    let mut frames = request.open(device)?;
    let native = frames.capture().backend() == BackendKind::Native;
    let mode = frames.capture().mode().format;
    println!(
        "{} via {}: {} {}x{} at {fps} fps",
        device.identity.display,
        frames.capture().backend(),
        mode.code,
        mode.resolution.width,
        mode.resolution.height
    );
    let mut stats = Preview::default();
    // Let AE and AWB settle on the preview.
    preview_for(&mut frames, &mut stats, 2.0, None);

    let t = Instant::now();
    let mut pending = frames.request_still(&StillRequest::jpeg(92).with_dng(native).settle(true));
    let still =
        preview_for(&mut frames, &mut stats, 10.0, Some(&mut pending)).ok_or("no still")??;
    let latency = t.elapsed();
    save(&still.shots[0], &dir, "still")?;
    println!(
        "still: request -> ready {:.1} ms, -> files {:.1} ms\n  {}",
        latency.as_secs_f64() * 1e3,
        t.elapsed().as_secs_f64() * 1e3,
        describe(&still.shots[0])
    );
    preview_for(&mut frames, &mut stats, 0.5, None);

    if native {
        let t = Instant::now();
        let mut pending = frames.request_still(
            &StillRequest::jpeg(92)
                .with_dng(true)
                .bracket([-1.0, 0.0, 1.0]),
        );
        let bracket = preview_for(&mut frames, &mut stats, 10.0, Some(&mut pending))
            .ok_or("no bracket")??;
        println!(
            "bracket: {} shots, request -> ready {:.1} ms",
            bracket.shots.len(),
            t.elapsed().as_secs_f64() * 1e3
        );
        for (i, shot) in bracket.shots.iter().enumerate() {
            save(shot, &dir, &format!("bracket{i}"))?;
            println!("bracket {i}: {}", describe(shot));
        }
    }
    preview_for(&mut frames, &mut stats, 1.0, None);
    drop(frames);
    let secs = (stats.last_ts.saturating_sub(stats.first_ts)) as f64 / 1e9;
    println!(
        "preview: {} frames in {secs:.2} s ({:.2} fps), {} sequence gaps, longest interval {:.1} ms",
        stats.frames,
        stats.frames.saturating_sub(1) as f64 / secs.max(1e-9),
        stats.gaps,
        stats.max_interval_ns as f64 / 1e6
    );
    println!("files in {}", dir.display());
    Ok(())
}
