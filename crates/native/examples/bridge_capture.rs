//! Streams raw frames from every bridged camera through `styx-native` directly: discovery, the
//! device graph, frame rates set exactly, exposure/gain through the control schedule, and the
//! values each frame reports.
//!
//! ```sh
//! STYX_SENSOR_PATH=/tmp/styx/ov9782.toml bridge_capture [frames-per-rate]
//! ```

use std::time::{Duration, Instant};

use styx_native::{NativeCamera, NativeError, NativeFrame, SensorLibrary, StreamSettings};

fn main() -> Result<(), NativeError> {
    let per_rate: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(120);
    let (cameras, problems) = styx_native::discover(&SensorLibrary::system());
    for p in problems {
        println!("problem: {p}");
    }
    for info in cameras {
        println!("{} [{}]", info.display_name(), info.key);
        for m in &info.modes {
            println!(
                "  mode {} {} {}x{} code {:#06x}: {:.3}..{:.3} fps (intervals {}/{}..{}/{} s)",
                m.mode,
                m.format,
                m.width,
                m.height,
                m.code,
                m.min_fps(),
                m.max_fps(),
                m.min_interval.num,
                m.min_interval.den,
                m.max_interval.num,
                m.max_interval.den
            );
        }
        println!("{}", info.graph.graph);
        let t_open = Instant::now();
        let mut cam = NativeCamera::open(info, Default::default())?;
        for fps in [30u32, 60, 120] {
            let t0 = Instant::now();
            let cfg = cam.configure(&StreamSettings::new(1280, 800).fps(fps))?;
            let configured = t0.elapsed();
            let mut stream = cam.start()?;
            let first = stream
                .next_blocking(Duration::from_secs(2))?
                .expect("a frame");
            let latency = first.dequeued - t0;
            let mut frames = vec![summary(&first)];
            drop(first);
            while frames.len() < per_rate {
                let f = stream
                    .next_blocking(Duration::from_secs(2))?
                    .expect("a frame");
                frames.push(summary(&f));
            }
            let (a, b) = (frames[1], frames[frames.len() - 1]);
            let measured = f64::from(b.0 - a.0) / (b.1 - a.1).as_secs_f64();
            println!(
                "{fps} fps: {} {} stride {}, interval {}/{} s ({:.4} fps predicted), measured {measured:.4} fps, configure {:.1} ms, configure->first frame {:.1} ms (open {:.1} ms before), {:?}, frame syncs/acks {:?}, embedded reports {:?}",
                cfg.fourcc,
                cfg.mode.mode,
                cfg.stride,
                cfg.interval.num,
                cfg.interval.den,
                cfg.interval.fps(),
                configured.as_secs_f64() * 1e3,
                latency.as_secs_f64() * 1e3,
                (t0 - t_open).as_secs_f64() * 1e3,
                stream.stats(),
                cam.event_counts(),
                cam.embedded_reports(),
            );
            if fps == 30 {
                controls(&cam, &mut stream)?;
            }
            cam.stop()?;
        }
        cam.close()?;
    }
    Ok(())
}

fn summary(f: &NativeFrame) -> (u32, Duration) {
    (f.sequence, f.timestamp)
}

fn mean(f: &NativeFrame) -> f64 {
    let d = f.data();
    let line = (f.width as usize) * 5 / 4;
    let mut sum = 0u64;
    let mut n = 0u64;
    for row in (0..f.height as usize).step_by(8) {
        for group in d[row * f.stride as usize..][..line].as_chunks::<5>().0 {
            sum += group[..4].iter().map(|&b| u64::from(b)).sum::<u64>();
            n += 4;
        }
    }
    sum as f64 * 4.0 / n.max(1) as f64
}

fn controls(cam: &NativeCamera, stream: &mut styx_native::FrameStream) -> Result<(), NativeError> {
    let c = cam.controls();
    println!(
        "  controls: fps {:?}, exposure {:?}, gain {:?}",
        c.fps_range(),
        c.exposure_range(),
        c.gain_range()
    );
    for (exposure, gain) in [
        (Duration::from_millis(20), 4.0),
        (Duration::from_millis(2), 1.0),
    ] {
        let mut landed = c.set_exposure(exposure)?;
        landed.extend(c.set_gain(gain)?);
        let target = landed.iter().map(|l| l.frame).max().unwrap_or(0);
        println!("  request exposure {exposure:?} gain {gain}: lands {landed:?}");
        loop {
            let f = stream
                .next_blocking(Duration::from_secs(2))?
                .expect("a frame");
            let fc = f.controls.expect("frame controls");
            if u64::from(f.sequence) + 3 >= target {
                println!(
                    "    frame {} exposure {:.3} ms gain {:.3} frame {:.3} ms verified {} level {:.1}{}",
                    f.sequence,
                    fc.exposure.as_secs_f64() * 1e3,
                    fc.gain(),
                    fc.frame_duration.as_secs_f64() * 1e3,
                    fc.verified,
                    mean(&f),
                    if u64::from(f.sequence) == target {
                        "  <- lands"
                    } else {
                        ""
                    }
                );
            }
            if u64::from(f.sequence) >= target + 3 {
                break;
            }
        }
    }
    Ok(())
}
