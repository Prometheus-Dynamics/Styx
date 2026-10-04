//! A region of interest cropped by a native camera's ISP, plus a low-resolution overview of
//! the whole frame: the PiSP's main output is the region at full resolution, its second output
//! the whole frame scaled down, both from one pass over each raw frame and handed out without
//! a copy. The region moves while frames flow; each frame says where it is
//! (`FrameMeta::crop`) and carries the overview (`FrameLease::overview`).
//!
//! Three ways to drive it:
//! - `plan`: `Frames::gray().roi(..).overview(..)`, moved with `Frames::roi` (the default);
//! - `service`: the same request through a camera service, moved with `FrameClient::set_roi`;
//! - `control`: a capture with `StyxConfig::native_crop` / `native_overview`, moved with the
//!   `OUTPUT_CROP` control.
//!
//! Prints, for each region, how many frames it took to apply, the frame and overview sizes,
//! and how well the region's pixels match the same region of the overview (mean luma), then
//! saves the last region and overview.
//!
//! ```sh
//! native_roi [plan|service|control] [frames-per-region] [out-dir]
//! ```

use std::io::Write;
use std::time::{Duration, Instant};

use styx::capture_api::native_controls as ctl;
use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;

type Error = Box<dyn std::error::Error>;

/// Frames whose region can move.
trait Source {
    fn set_region(&mut self, region: Option<FrameRect>) -> Result<(), Error>;
    fn next(&mut self) -> Result<FrameLease, Error>;
}

fn frame(outcome: RecvOutcome<FrameLease>) -> Result<FrameLease, Error> {
    match outcome {
        RecvOutcome::Data(f) => Ok(f),
        _ => Err("no frame".into()),
    }
}

impl Source for Frames {
    fn set_region(&mut self, region: Option<FrameRect>) -> Result<(), Error> {
        self.roi().set(region);
        Ok(())
    }
    fn next(&mut self) -> Result<FrameLease, Error> {
        frame(self.next_frame(Duration::from_secs(3)))
    }
}

impl Source for FrameClient {
    fn set_region(&mut self, region: Option<FrameRect>) -> Result<(), Error> {
        Ok(self.set_roi(region)?)
    }
    fn next(&mut self) -> Result<FrameLease, Error> {
        frame(self.recv(Duration::from_secs(3)))
    }
}

impl Source for CaptureHandle {
    fn set_region(&mut self, region: Option<FrameRect>) -> Result<(), Error> {
        let r = region.unwrap_or(FrameRect::new(0, 0, 0, 0));
        let rect = ControlRect {
            x: r.x as i32,
            y: r.y as i32,
            width: r.width,
            height: r.height,
        };
        Ok(self.set_control(ctl::OUTPUT_CROP, ControlValue::Rect(rect))?)
    }
    fn next(&mut self) -> Result<FrameLease, Error> {
        frame(self.recv_blocking(Duration::from_secs(3)))
    }
}

fn mean_luma(f: &FrameLease, r: FrameRect) -> f64 {
    let planes = f.planes();
    let p = &planes[0];
    let mut sum = 0u64;
    for y in r.y..r.y + r.height {
        let row = &p.data()[y as usize * p.stride()..];
        sum += row[r.x as usize..(r.x + r.width) as usize]
            .iter()
            .map(|&v| u64::from(v))
            .sum::<u64>();
    }
    sum as f64 / f64::from(r.width * r.height)
}

/// Whether `outer` covers `inner` (the ISP rounds regions out to even pixels).
fn contains(outer: FrameRect, inner: FrameRect) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && outer.x + outer.width >= inner.x + inner.width
        && outer.y + outer.height >= inner.y + inner.height
}

fn whole(f: &FrameLease) -> FrameRect {
    let r = f.meta().format.resolution;
    FrameRect::new(0, 0, r.width.get(), r.height.get())
}

fn save_pgm(path: &str, f: &FrameLease) -> std::io::Result<()> {
    let r = whole(f);
    let planes = f.planes();
    let p = &planes[0];
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(out, "P5\n{} {}\n255\n", r.width, r.height)?;
    for y in 0..r.height as usize {
        out.write_all(&p.data()[y * p.stride()..y * p.stride() + r.width as usize])?;
    }
    out.flush()
}

/// Moves the region through `regions` (the first set at the start), checking each.
fn sweep(
    source: &mut dyn Source,
    (w, h): (u32, u32),
    regions: &[FrameRect],
    per_region: usize,
    out: &str,
) -> Result<(), Error> {
    let mut last = None;
    for (i, &region) in regions.iter().enumerate() {
        if i > 0 {
            source.set_region(Some(region))?;
        }
        let set = Instant::now();
        let (mut applied, mut worst, mut crop) = (None, 0.0f64, None);
        for n in 0..per_region {
            let f = source.next()?;
            let Some(c) = f.meta().crop else {
                // Frames from before the region applied.
                continue;
            };
            if applied.is_none() && contains(c, region) {
                applied = Some((n, set.elapsed()));
            }
            if applied.is_some() {
                let o = f.overview().ok_or("frame without its overview")?;
                let or = whole(o);
                let scaled = c.scaled((w, h), (or.width, or.height));
                if let Some(scaled) = scaled.clipped_to(or.width, or.height) {
                    worst = worst.max((mean_luma(&f, whole(&f)) - mean_luma(o, scaled)).abs());
                }
            }
            crop = Some(c);
            last = Some(f);
        }
        let f = last.as_ref().ok_or("no frame")?;
        println!(
            "region {region:?}: crop {crop:?} applied after {applied:?}, frame {}x{}, overview {:?}, \
             worst mean-luma difference to the overview {worst:.1}",
            whole(f).width,
            whole(f).height,
            f.overview().map(|o| (whole(o).width, whole(o).height)),
        );
    }
    source.set_region(None)?;
    let full = loop {
        let f = source.next()?;
        if f.meta().crop.is_none() {
            break f;
        }
    };
    println!("region cleared: frame {:?}", whole(&full));
    if let Some(f) = last {
        save_pgm(&format!("{out}/native-roi-region.pgm"), &f)?;
        if let Some(o) = f.overview() {
            save_pgm(&format!("{out}/native-roi-overview.pgm"), o)?;
        }
        println!("saved {out}/native-roi-region.pgm and {out}/native-roi-overview.pgm");
    }
    Ok(())
}

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    let how = args.get(1).map_or("plan", String::as_str);
    let per_region: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(30);
    let out = args.get(3).cloned().unwrap_or_else(|| "/tmp".into());
    let devices = probe_all();
    let (device, mode) = devices
        .iter()
        .find_map(|d| {
            let b = d.backends.iter().find(|b| b.kind == BackendKind::Native)?;
            let mode = b
                .descriptor
                .modes
                .iter()
                .filter(|m| m.format.code == FourCc::NV12)
                .max_by_key(|m| m.format.resolution.width.get())?;
            Some((d.clone(), mode.clone()))
        })
        .ok_or("no native camera with processed modes")?;
    let size = (
        mode.format.resolution.width.get(),
        mode.format.resolution.height.get(),
    );
    let (w, h) = size;
    println!("{} NV12 {w}x{h}, driven by {how}", device.identity.display);
    let regions = [
        FrameRect::new(w / 2 - 160, h / 2 - 100, 320, 200),
        FrameRect::new(0, 0, 256, 256),
        FrameRect::new(w - 400, h - 300, 400, 300),
        FrameRect::new(101, 51, 63, 41),
    ];
    let request = Frames::gray()
        .size_at_least(w, h)
        .roi(regions[0])
        .overview(w / 4, h / 4);
    match how {
        "control" => {
            let config = StyxConfig::default()
                .native_crop(regions[0])
                .native_overview(w / 4, h / 4);
            let mut handle = CaptureRequest::new(&device)
                .backend(BackendKind::Native)
                .mode(mode.id.clone())
                .config(config)
                .start()?;
            sweep(&mut handle, size, &regions, per_region, &out)?;
            handle.stop();
        }
        "service" => {
            let socket = format!("{out}/native-roi.sock");
            let _service = CameraService::new(device.clone()).serve(&socket)?;
            let mut client = FrameClient::request(&socket, &request)?;
            println!("delivered: {:?}", client.delivered());
            sweep(&mut client, size, &regions, per_region, &out)?;
        }
        _ => {
            let mut frames = request.open(&device)?;
            print!("{}", frames.plan());
            println!("delivered: {:?}", frames.plan().delivered());
            sweep(&mut frames, size, &regions, per_region, &out)?;
            frames.stop();
        }
    }
    Ok(())
}
