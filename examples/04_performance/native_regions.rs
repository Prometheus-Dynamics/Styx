//! Several regions of interest from a native camera's PiSP: the main output's crop, the second
//! output's, and extra back end passes over the same raw frame, each region a
//! `CompanionKind::Region` companion of the frame it was cut from.
//!
//! ```sh
//! native_regions bench N [frames]   one consumer, N regions (1 = the main output's crop only)
//!                                   moving a little every frame: CPU and latency per frame
//! native_regions views [frames]     the same with a viewer of the whole frame sharing the
//!                                   capture (gray regions are then views): the old way
//! native_regions pixels [frames]    a viewer, an NV12 tracker (second output + passes) and a
//!                                   gray tracker: each ISP region against the same pixels of
//!                                   the viewer's frame from the same capture
//! native_regions trackers [frames]  two trackers, no viewer: the first's region the main
//!                                   output's crop, the second's a pass, both against the overview
//! native_regions stale [frames]     regions jumping every 4 frames, with and without
//!                                   `skip_stale_regions`: frames whose crops miss the region
//! native_regions service [frames]   two camera service clients with two regions each
//! ```

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use styx::ipc::{CameraService, FrameClient};
use styx::planner::plan_many;
use styx::prelude::*;

type Error = Box<dyn std::error::Error>;

fn next(frames: &mut Frames) -> Result<FrameLease, Error> {
    match frames.next_frame(Duration::from_secs(3)) {
        RecvOutcome::Data(f) => Ok(f),
        _ => Err("no frame".into()),
    }
}

fn size(f: &FrameLease) -> (u32, u32) {
    let r = f.meta().format.resolution;
    (r.width.get(), r.height.get())
}

/// Process CPU time so far.
fn cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid timespec for the call to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// `CLOCK_MONOTONIC` now, in nanoseconds (frame timestamps' clock).
fn now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: as above.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Luma of `f` (a GREY or NV12 frame) at `(x, y)`.
fn luma(f: &FrameLease, x: u32, y: u32) -> u8 {
    let planes = f.planes();
    let p = &planes[0];
    p.data()[y as usize * p.stride() + x as usize]
}

/// Mean |a − b| of region frame `region` (at `crop` in the full frame) against `full`.
fn diff(region: &FrameLease, full: &FrameLease) -> (f64, u8) {
    let Some(c) = region.meta().crop else {
        return (f64::NAN, 0);
    };
    let (w, h) = size(region);
    let (mut sum, mut max, mut n) = (0u64, 0u8, 0u64);
    for y in 0..h {
        for x in 0..w {
            let d = luma(region, x, y).abs_diff(luma(full, c.x + x, c.y + y));
            sum += u64::from(d);
            max = max.max(d);
            n += 1;
        }
    }
    (sum as f64 / n.max(1) as f64, max)
}

/// `outer` contains `inner`.
fn covers(outer: FrameRect, inner: FrameRect) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && outer.x + outer.width >= inner.x + inner.width
        && outer.y + outer.height >= inner.y + inner.height
}

/// Mean luma of `f` within `r`.
fn mean(f: &FrameLease, r: FrameRect) -> f64 {
    let mut sum = 0u64;
    for y in r.y..r.y + r.height {
        for x in r.x..r.x + r.width {
            sum += u64::from(luma(f, x, y));
        }
    }
    sum as f64 / f64::from(r.width * r.height).max(1.0)
}

fn camera() -> Result<(ProbedDevice, (u32, u32)), Error> {
    probe_all()
        .into_iter()
        .find_map(|d| {
            let b = d.backends.iter().find(|b| b.kind == BackendKind::Native)?;
            let m = b
                .descriptor
                .modes
                .iter()
                .filter(|m| m.format.code == FourCc::NV12)
                .max_by_key(|m| m.format.resolution.width.get())?;
            let r = m.format.resolution;
            Some((d.clone(), (r.width.get(), r.height.get())))
        })
        .ok_or_else(|| "no native camera with processed modes".into())
}

/// `n` 128x128 regions spread over a `w`x`h` frame, moved `step` pixels right.
fn regions(n: usize, (w, h): (u32, u32), step: u32) -> Vec<FrameRect> {
    (0..n as u32)
        .map(|i| {
            let x = (64 + i * (w - 256) / n.max(1) as u32 + step) % (w - 128);
            FrameRect::new(x & !1, ((h / 4 + i * 97) % (h - 128)) & !1, 128, 128)
        })
        .collect()
}

/// Frames of `frames` with its regions moving 2 px a frame: CPU per frame, latency, regions
/// per frame.
fn bench(mut frames: Frames, n: usize, dims: (u32, u32), count: usize) -> Result<(), Error> {
    print!("{}", frames.plan());
    println!("delivered: {:?}", frames.plan().delivered().regions);
    for _ in 0..30 {
        next(&mut frames)?;
    }
    let roi = frames.roi();
    let (cpu0, t0) = (cpu(), Instant::now());
    let mut lat = Vec::new();
    let mut got = 0;
    for i in 0..count {
        roi.set_regions(&regions(n, dims, 2 * i as u32));
        let f = next(&mut frames)?;
        lat.push(now_ns().saturating_sub(f.meta().timestamp));
        got += f.regions().count();
    }
    let (cpu, wall) = (cpu() - cpu0, t0.elapsed());
    lat.sort();
    let ms = |ns: u64| ns as f64 / 1e6;
    println!(
        "{n} regions: {:.3} ms CPU per frame ({:.1}% of a core), latency median {:.2} ms p95 {:.2} \
         ms, {:.2} regions per frame, {:.1} fps",
        cpu.as_secs_f64() * 1e3 / count as f64,
        cpu.as_secs_f64() / wall.as_secs_f64() * 100.0,
        ms(lat[lat.len() / 2]),
        ms(lat[lat.len() * 95 / 100]),
        got as f64 / count as f64,
        count as f64 / wall.as_secs_f64(),
    );
    frames.stop();
    Ok(())
}

/// Takes frames from `a` and `b` until each has one from the same capture (timestamp).
fn paired(a: &mut Frames, b: &mut Frames) -> Result<(FrameLease, FrameLease), Error> {
    let mut seen: VecDeque<FrameLease> = VecDeque::new();
    for _ in 0..20 {
        let fb = next(b)?;
        for _ in 0..4 {
            let ts = fb.meta().timestamp;
            if let Some(i) = seen.iter().position(|f| f.meta().timestamp == ts) {
                let fa = seen.remove(i).ok_or("frame")?;
                return Ok((fa, fb));
            }
            seen.push_back(next(a)?);
            if seen.len() > 6 {
                seen.pop_front();
            }
        }
    }
    Err("no frames from the same capture".into())
}

fn pixels(device: &ProbedDevice, dims: (u32, u32), count: usize) -> Result<(), Error> {
    let r = regions(4, dims, 0);
    let plan = plan_many(
        device,
        &[
            Frames::nv12(),
            Frames::nv12().regions([r[0], r[1], r[2]]),
            Frames::gray().regions([r[3]]),
        ],
    )?;
    print!("{plan}");
    for c in &plan.consumers {
        println!("delivered: {:?}", c.delivered().regions);
    }
    let mut all = plan.start()?.into_iter();
    let (mut viewer, mut nv12, mut gray) = (
        all.next().ok_or("viewer")?,
        all.next().ok_or("nv12")?,
        all.next().ok_or("gray")?,
    );
    for _ in 0..30 {
        next(&mut viewer)?;
        next(&mut nv12)?;
        next(&mut gray)?;
    }
    let mut worst = [(0.0f64, 0u8); 4];
    for _ in 0..count {
        let (full, tracked) = paired(&mut viewer, &mut nv12)?;
        for (i, w) in worst.iter_mut().take(3).enumerate() {
            let region = tracked.region(i as u8).ok_or("region missing")?;
            let (d, m) = diff(region, &full);
            *w = (w.0.max(d), w.1.max(m));
        }
        let (full, view) = paired(&mut viewer, &mut gray)?;
        let (d, m) = diff(&view, &full);
        worst[3] = (worst[3].0.max(d), worst[3].1.max(m));
    }
    for (i, (d, m)) in worst.iter().enumerate() {
        println!(
            "region {i}: against the viewer's frame from the same capture: worst mean |diff| \
             {d:.3}, max {m}"
        );
    }
    Ok(())
}

fn trackers(device: &ProbedDevice, dims: (u32, u32), count: usize) -> Result<(), Error> {
    let r = regions(2, dims, 0);
    let (w, h) = dims;
    let plan = plan_many(
        device,
        &[
            Frames::gray().roi(r[0]).overview(w / 4, h / 4),
            Frames::gray().roi(r[1]).overview(w / 4, h / 4),
        ],
    )?;
    print!("{plan}");
    let mut all = plan.start()?.into_iter();
    let (mut a, mut b) = (all.next().ok_or("a")?, all.next().ok_or("b")?);
    let mut worst = [0.0f64; 2];
    for _ in 0..count {
        for (i, frames) in [&mut a, &mut b].into_iter().enumerate() {
            let f = next(frames)?;
            let (Some(c), Some(o)) = (f.meta().crop, f.overview()) else {
                continue;
            };
            let (ow, oh) = size(o);
            let scaled = c.scaled(dims, (ow, oh)).clipped_to(ow, oh).ok_or("crop")?;
            let (fw, fh) = size(&f);
            let d = (mean(&f, FrameRect::new(0, 0, fw, fh)) - mean(o, scaled)).abs();
            worst[i] = worst[i].max(d);
        }
    }
    println!(
        "trackers: worst mean-luma difference of each region to the overview: {:.2} (main \
         output crop), {:.2} (extra pass)",
        worst[0], worst[1]
    );
    Ok(())
}

fn stale(device: &ProbedDevice, dims: (u32, u32), count: usize) -> Result<(), Error> {
    for skip in [false, true] {
        let first = regions(2, dims, 0);
        let mut req = Frames::gray().regions(first.clone());
        if skip {
            req = req.skip_stale_regions();
        }
        let mut frames = req.open(device)?;
        for _ in 0..20 {
            next(&mut frames)?;
        }
        let roi = frames.roi();
        let (mut stale, mut total) = (0, 0);
        let mut current = first;
        let t0 = Instant::now();
        for i in 0..count {
            if i % 4 == 0 {
                // Jump: the new regions do not overlap the old ones.
                current = regions(2, dims, 300 * (i as u32 / 4 % 3));
                roi.set_regions(&current);
            }
            let f = next(&mut frames)?;
            total += 1;
            let miss = current.iter().enumerate().any(|(k, want)| {
                f.region(k as u8)
                    .and_then(|r| r.meta().crop)
                    .is_none_or(|c| !covers(c, *want))
            });
            stale += usize::from(miss);
        }
        println!(
            "skip_stale_regions {skip}: {stale} of {total} frames show stale regions ({:.1} fps)",
            total as f64 / t0.elapsed().as_secs_f64()
        );
        frames.stop();
    }
    Ok(())
}

fn service(device: &ProbedDevice, dims: (u32, u32), count: usize) -> Result<(), Error> {
    let socket = "/tmp/native-regions.sock";
    let _service = CameraService::new(device.clone()).serve(socket)?;
    let r = regions(4, dims, 0);
    let a = FrameClient::request(socket, &Frames::nv12().regions([r[0], r[1]]))?;
    let b = FrameClient::request(socket, &Frames::nv12().regions([r[2], r[3]]))?;
    println!("a: {:?}\nb: {:?}", a.delivered(), b.delivered());
    let mut ok = 0;
    for i in 0..count {
        let moved = regions(4, dims, 4 * i as u32);
        a.set_regions(&moved[..2])?;
        b.set_regions(&moved[2..])?;
        for (client, want) in [(&a, &moved[..2]), (&b, &moved[2..])] {
            let RecvOutcome::Data(f) = client.recv(Duration::from_secs(3)) else {
                return Err("no frame".into());
            };
            let crops: Vec<_> = f.regions().map(|(_, r)| r.meta().crop).collect();
            ok += usize::from(crops.len() == 2 && crops.iter().all(Option::is_some));
            if i + 1 == count {
                println!("last frame's regions {crops:?}, set {want:?}");
            }
        }
    }
    println!("service: {ok} of {} frames carried both regions", 2 * count);
    Ok(())
}

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    let how = args.get(1).map_or("bench", String::as_str);
    let (device, dims) = camera()?;
    println!("{} NV12 {}x{}", device.identity.display, dims.0, dims.1);
    match how {
        "bench" => {
            let n: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(1);
            let count = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(300);
            let frames = Frames::gray()
                .regions(regions(n, dims, 0))
                .overview(dims.0 / 4, dims.1 / 4)
                .open(&device)?;
            bench(frames, n, dims, count)
        }
        "views" => {
            let n: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(1);
            let count = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(300);
            let plan = plan_many(
                &device,
                &[
                    Frames::gray()
                        .regions(regions(n, dims, 0))
                        .overview(dims.0 / 4, dims.1 / 4),
                    Frames::nv12(),
                ],
            )?;
            let mut all = plan.start()?;
            let viewer = all.pop().ok_or("viewer")?;
            let frames = all.pop().ok_or("tracker")?;
            // The viewer takes nothing: the tracker pulls the capture alone.
            let r = bench(frames, n, dims, count);
            drop(viewer);
            r
        }
        "pixels" => pixels(
            &device,
            dims,
            args.get(2).and_then(|a| a.parse().ok()).unwrap_or(60),
        ),
        "trackers" => trackers(
            &device,
            dims,
            args.get(2).and_then(|a| a.parse().ok()).unwrap_or(150),
        ),
        "stale" => stale(
            &device,
            dims,
            args.get(2).and_then(|a| a.parse().ok()).unwrap_or(120),
        ),
        "service" => service(
            &device,
            dims,
            args.get(2).and_then(|a| a.parse().ok()).unwrap_or(60),
        ),
        other => Err(format!("unknown mode {other}").into()),
    }
}
