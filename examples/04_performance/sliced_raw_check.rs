//! On a device: a native camera service with sliced delivery, lazy back end buffers and raw
//! companions (`native_slice_line`, `native_deliver_slices`, `native_lazy_output_buffers`,
//! `native_sensor_raw`): a 120 fps NV12 vision client, a low-priority raw client joining the
//! running capture (it must get frames), then rotation 180 (the picture must turn).
//!
//! ```text
//! sliced_raw_check [whole]    whole: the same without slicing
//! ```
use std::time::{Duration, Instant};
use styx::ipc::{CameraService, FrameClient, SERVICE_ROTATION};
use styx::prelude::*;

fn luma(f: &FrameLease) -> Vec<u8> {
    let fmt = f.meta().format;
    let (w, h) = (
        fmt.resolution.width.get() as usize,
        fmt.resolution.height.get() as usize,
    );
    let planes = f.planes();
    let p = &planes[0];
    let stride = f.layout_slice()[0].stride;
    (0..h)
        .flat_map(|y| p.data()[y * stride..y * stride + w].to_vec())
        .collect()
}

fn corr(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len() as f64;
    let (ma, mb) = (
        a.iter().map(|&v| v as f64).sum::<f64>() / n,
        b.iter().map(|&v| v as f64).sum::<f64>() / n,
    );
    let (mut s, mut sa, mut sb) = (0.0, 0.0, 0.0);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (x as f64 - ma, y as f64 - mb);
        s += x * y;
        sa += x * x;
        sb += y * y;
    }
    s / (sa * sb).sqrt()
}

/// Frames for `secs`: count, and the last one's luma (or raw format and size), copied.
fn read(c: &FrameClient, secs: f64) -> (u32, Option<(Vec<u8>, FourCc, usize)>) {
    let (t, mut n, mut last) = (Instant::now(), 0, None);
    while t.elapsed().as_secs_f64() < secs {
        if let RecvOutcome::Data(f) = c.recv(Duration::from_millis(200)) {
            n += 1;
            let code = f.meta().format.code;
            let bytes = f.planes().iter().map(|p| p.data().len()).sum::<usize>();
            let l = if code == FourCc::NV12 {
                luma(&f)
            } else {
                Vec::new()
            };
            last = Some((l, code, bytes));
        }
    }
    (n, last)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sliced = std::env::args().nth(1).as_deref() != Some("whole");
    let path = std::env::temp_dir().join(format!("styx-sliced-raw-{}.sock", std::process::id()));
    let path = path.as_path();
    let camera = probe_all().into_iter().next().ok_or("no camera")?;
    let config = StyxConfig::default()
        .native_sensor_raw(true)
        .native_lazy_output_buffers(true)
        .native_slice_line(sliced.then_some(400))
        .native_deliver_slices(sliced);
    let _service = CameraService::new(camera)
        .capture_config(config)
        .keep_streaming()
        .serve(path)?;
    let vision = std::sync::Arc::new(
        FrameClient::options(path).request(&Frames::nv12().size(1280, 800).fps(120))?,
    );
    println!(
        "sliced {sliced}; vision delivered {:?}",
        vision.delivered().map(|d| (d.format, d.size, d.fps))
    );
    // The vision client reads all along: the latest luma and the frame count.
    let latest = std::sync::Arc::new(std::sync::Mutex::new((0u32, Vec::new())));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let (vision, latest, stop) = (vision.clone(), latest.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let RecvOutcome::Data(f) = vision.recv(Duration::from_millis(200)) {
                    let l = luma(&f);
                    drop(f);
                    let mut g = latest.lock().unwrap();
                    g.0 += 1;
                    g.1 = l;
                }
            }
        })
    };
    let count = || latest.lock().unwrap().0;
    std::thread::sleep(Duration::from_secs(2));
    println!("vision {:.1} fps", count() as f64 / 2.0);
    let raw = FrameClient::options(path)
        .low_priority()
        .timeout(Duration::from_secs(5))
        .request(&Frames::sensor_raw().every_frame(8))?;
    println!(
        "raw delivered {:?}",
        raw.delivered().map(|d| (d.format, d.size))
    );
    let (rn, rl) = read(&raw, 3.0);
    println!(
        "raw {:.1} fps, last {:?}",
        rn as f64 / 3.0,
        rl.map(|(_, c, b)| (c, b))
    );
    drop(raw);
    let a = latest.lock().unwrap().1.clone();
    let applied = match vision.set_control(SERVICE_ROTATION, ControlValue::Int(180)) {
        Ok(applied) => applied,
        Err(e) => {
            println!("rotation: {e}");
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            reader.join().unwrap();
            return Ok(());
        }
    };
    println!("rotation: {applied:?}");
    let n0 = count();
    std::thread::sleep(Duration::from_secs(3));
    let n = count() - n0;
    let b = latest.lock().unwrap().1.clone();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    reader.join().unwrap();
    let rot: Vec<u8> = b.iter().rev().copied().collect();
    println!(
        "after rotation {:.1} fps; corr as-is {:.3} turned {:.3}",
        n as f64 / 3.0,
        corr(&a, &b),
        corr(&a, &rot)
    );
    Ok(())
}
