//! Per-output crops in libcamera's Raspberry Pi pipeline (`rpi::ScalerCrops`): the ISP's main
//! output shows one rectangle of the sensor image and its second output another (here the whole
//! field of view), each scaled to its stream's size. Checks that the libcamera in use exposes
//! the control, moves the main output's crop while frames flow, and counts the frames until
//! each crop shows (in the request metadata, and in the pixels: the main output's mean luma
//! against the same region of the second output).
//!
//! `plan` does the same through the planner: `Frames::gray().roi(..).overview(..)` on the
//! libcamera backend, moved with `Frames::roi`, each frame saying which region it shows
//! (`FrameMeta::crop`).
//!
//! ```sh
//! libcamera_crops [control|plan] [frames-per-step]
//! ```

use std::time::{Duration, Instant};

use styx::prelude::*;

type Error = Box<dyn std::error::Error>;

/// The four numbers of a rectangle property as libcamera prints it.
fn rect_numbers(s: &str) -> Option<[u32; 4]> {
    let n: Vec<u32> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse().ok())
        .collect();
    (n.len() >= 4).then(|| [n[0], n[1], n[2], n[3]])
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
    sum as f64 / f64::from(r.width * r.height).max(1.0)
}

fn size(f: &FrameLease) -> (u32, u32) {
    let r = f.meta().format.resolution;
    (r.width.get(), r.height.get())
}

/// The planner's region of interest on the libcamera backend.
fn plan(device: &ProbedDevice, (fw, fh): (u32, u32), per_step: usize) -> Result<(), Error> {
    let regions = [
        FrameRect::new(fw / 2 - 160, fh / 2 - 100, 320, 200),
        FrameRect::new(0, 0, 320, 200),
        FrameRect::new(fw - 320, fh - 200, 320, 200),
        // Another size: scaled to the first region's.
        FrameRect::new(fw / 4, fh / 4, fw / 2, fh / 2),
    ];
    let mut frames = Frames::gray()
        .backend(BackendKind::Libcamera)
        .roi(regions[0])
        .overview(320, 200)
        .open(device)?;
    print!("{}", frames.plan());
    println!("delivered: {:?}", frames.plan().delivered());
    let roi = frames.roi();
    for (i, &region) in regions.iter().enumerate() {
        if i > 0 {
            roi.set(Some(region));
        }
        let set = Instant::now();
        let (mut applied, mut worst, mut last) = (None, 0.0f64, None);
        for n in 0..per_step {
            let RecvOutcome::Data(f) = frames.next_frame(Duration::from_secs(3)) else {
                return Err("no frame".into());
            };
            let crop = f.meta().crop;
            if applied.is_none() && crop == Some(region) {
                applied = Some((n, set.elapsed()));
            }
            if applied.is_some() {
                let o = f.overview().ok_or("frame without its overview")?;
                let (ow, oh) = size(o);
                let scaled = region.scaled((fw, fh), (ow, oh));
                let whole = FrameRect::new(0, 0, size(&f).0, size(&f).1);
                worst = worst.max((mean_luma(&f, whole) - mean_luma(o, scaled)).abs());
            }
            last = Some((size(&f), crop));
        }
        println!(
            "region {region:?}: applied after {applied:?}; frames {last:?}; worst mean-luma \
             difference to the overview {worst:.1}"
        );
    }
    frames.stop();
    Ok(())
}

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    let how = args.get(1).map_or("control", String::as_str);
    let per_step: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(20);
    let devices = probe_all();
    let (device, backend) = devices
        .iter()
        .find_map(|d| {
            let b = d.backends.iter().find(|b| {
                b.kind == BackendKind::Libcamera
                    && b.descriptor
                        .controls
                        .iter()
                        .any(|c| c.name == "ScalerCrops")
            })?;
            Some((d.clone(), b.clone()))
        })
        .ok_or("no libcamera camera with the rpi::ScalerCrops control")?;
    println!("{} through libcamera", device.identity.display);
    for (k, v) in &backend.properties {
        if k.contains("Crop") || k.contains("PixelArray") || k == "Model" {
            println!("  property {k}: {v}");
        }
    }
    let crops = backend
        .descriptor
        .controls
        .iter()
        .find(|c| c.name == "ScalerCrops")
        .ok_or("ScalerCrops")?;
    println!(
        "  control ScalerCrops: id {:?}, {:?}",
        crops.id, crops.access
    );
    let crop_id = crops.id;
    // The sensor area of the full field of view.
    let max = backend
        .properties
        .iter()
        .find(|(k, _)| k == "ScalerCropMaximum")
        .and_then(|(_, v)| rect_numbers(v));
    let mode = backend
        .descriptor
        .modes
        .iter()
        .filter(|m| m.format.code == FourCc::NV12)
        .max_by_key(|m| m.format.resolution.width.get())
        .ok_or("no NV12 mode")?
        .clone();
    let (fw, fh) = (
        mode.format.resolution.width.get(),
        mode.format.resolution.height.get(),
    );
    if how == "plan" {
        return plan(&device, (fw, fh), per_step);
    }
    let (ow, oh) = (320, 200);
    let config = StyxConfig::default()
        .libcamera_output_size(ow, oh)
        .libcamera_second_output(ow, oh);
    let handle = CaptureRequest::new(&device)
        .backend(BackendKind::Libcamera)
        .mode(mode.id.clone())
        .config(config)
        .start()?;
    let next = || match handle.recv_blocking(Duration::from_secs(3)) {
        RecvOutcome::Data(f) => Ok::<_, Error>(f),
        _ => Err("no frame".into()),
    };
    let first = next()?;
    println!(
        "mode {fw}x{fh}; main {:?}, second {:?}; ScalerCropMaximum {max:?}; ScalerCrops now {:?}",
        size(&first),
        first.companions().next().map(|(k, c)| (k, size(c))),
        handle.get_control(crop_id)
    );
    // The probe's `ScalerCropMaximum` is empty until the camera is configured; the crops in
    // the first frames' metadata are the whole field of view.
    let start = match handle.get_control(crop_id) {
        Ok(ControlValue::Rects(r)) => r
            .first()
            .map(|r| [r.x as u32, r.y as u32, r.width, r.height]),
        _ => None,
    };
    let [mx, my, mw, mh] = max
        .filter(|m| m[2] > 0 && m[3] > 0)
        .or(start)
        .unwrap_or([0, 0, fw, fh]);
    println!("sensor area of the frame: ({mx}, {my}) {mw}x{mh}");
    // Frame rectangles (of the mode's frame) in sensor coordinates.
    let sensor = |r: FrameRect| ControlRect {
        x: (mx + r.x * mw / fw) as i32,
        y: (my + r.y * mh / fh) as i32,
        width: r.width * mw / fw,
        height: r.height * mh / fh,
    };
    let whole = sensor(FrameRect::new(0, 0, fw, fh));
    let regions = [
        FrameRect::new(fw / 2 - 160, fh / 2 - 100, 320, 200),
        FrameRect::new(0, 0, 320, 200),
        FrameRect::new(fw - 640, fh - 400, 640, 400),
        FrameRect::new(fw / 4, fh / 4, fw / 2, fh / 2),
    ];
    for region in regions {
        let want = sensor(region);
        handle.set_control(crop_id, ControlValue::Rects(vec![want, whole]))?;
        let set = Instant::now();
        let (mut by_meta, mut by_pixels) = (None, None);
        let mut worst = 0.0f64;
        for n in 0..per_step {
            let f = next()?;
            let applied = handle.get_control(crop_id).ok();
            if by_meta.is_none()
                && let Some(ControlValue::Rects(r)) = &applied
                && r.first() == Some(&want)
            {
                by_meta = Some((n, set.elapsed()));
            }
            let Some((_, o)) = f.companions().next() else {
                return Err("frame without the second output".into());
            };
            let (sw, sh) = size(o);
            let scaled = FrameRect::new(
                region.x * sw / fw,
                region.y * sh / fh,
                (region.width * sw / fw).max(1),
                (region.height * sh / fh).max(1),
            );
            let diff = (mean_luma(&f, FrameRect::new(0, 0, size(&f).0, size(&f).1))
                - mean_luma(o, scaled))
            .abs();
            if by_pixels.is_none() && diff < 2.0 && by_meta.is_some() {
                by_pixels = Some(n);
            }
            if by_pixels.is_some() {
                worst = worst.max(diff);
            }
        }
        println!(
            "crop {region:?} (sensor {want:?}): metadata after {by_meta:?}, pixels after \
             {by_pixels:?} frames; worst mean-luma difference to the second output since {worst:.1}"
        );
    }
    handle.stop();
    Ok(())
}
