//! `pisp` and `soft`: the loop on the native camera.

use std::time::{Duration, Instant};

use styx_native::{BufferMemory, CameraOptions, NativeCamera, SensorLibrary, StreamSettings};
use styx_pipeline::device::{
    PispOptions, PispPipeline, PispTimes, SoftPipeline, process_usage, thread_usage,
};
use styx_pipeline::measure::{grey_ratios, nv12_to_rgb, plane_mean, write_pgm, write_ppm};
use styx_pipeline::rawrec::{Header, RawWriter, VERSION};
use styx_softisp::{OutputBuffers, Scale};

use crate::replay_run::luma_of_rgb;
use crate::report::{FrameLog, Summary, write_csv};
use crate::{Args, controls_for, monotonic, tuning};

const TIMEOUT: Duration = Duration::from_secs(2);

fn open_camera(a: &Args) -> Result<(NativeCamera, Instant), String> {
    let mut lib = SensorLibrary::system();
    if let Some(d) = &a.description {
        lib = lib.with_path_first(d.clone());
    }
    let (cameras, problems) = styx_native::discover(&lib);
    for p in problems {
        println!("discovery: {p}");
    }
    let info = cameras
        .into_iter()
        .next()
        .ok_or("no bridged camera found")?;
    println!("camera: {} [{}]", info.display_name(), info.key);
    let opened = Instant::now();
    let mut options = CameraOptions::default();
    if let Some(heap) = &a.heap {
        options.memory = BufferMemory::DmaHeap(heap.clone());
    }
    let cam = NativeCamera::open(info, options).map_err(|e| e.to_string())?;
    Ok((cam, opened))
}

fn settings(a: &Args) -> StreamSettings {
    StreamSettings::new(1280, 800).fps(a.fps.round() as u32)
}

/// Phase means of a raw frame's 2x2 cells (0..1), to check the colour filter order: the two
/// greens are the pair of cells with nearly equal means.
fn phase_means(
    raw: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    packing: styx_softisp::RawPacking,
) -> [f64; 4] {
    let mut row = vec![0u16; w];
    let mut sums = [0.0f64; 4];
    let full = f64::from((1u32 << packing.bit_depth()) - 1);
    for y in (0..h).step_by(2).flat_map(|y| [y, y + 1]) {
        styx_pipeline::rawrec::unpack_row(packing, &raw[y * stride..], w, &mut row);
        for (x, &v) in row.iter().enumerate() {
            sums[(y & 1) * 2 + (x & 1)] += f64::from(v);
        }
    }
    let n = (w * h / 4) as f64;
    sums.map(|s| s / n / full)
}

pub fn soft(a: &Args) -> Result<(), String> {
    let tuning = tuning(a)?;
    let (cam, opened) = open_camera(a)?;
    let mut p =
        SoftPipeline::open(cam, &settings(a), &tuning, a.threads).map_err(|e| e.to_string())?;
    let cfg = p.configured().clone();
    println!(
        "soft: {} {}x{} stride {} at {:.3} fps",
        cfg.fourcc,
        cfg.mode.width,
        cfg.mode.height,
        cfg.stride,
        cfg.interval.fps()
    );
    let format = p.soft_loop().format();
    let mut writer = match &a.record {
        Some(base) => Some(
            RawWriter::create(
                base,
                &Header {
                    styx_raw_recording: VERSION,
                    format,
                    stride: cfg.stride as usize,
                    sensor: p.soft_loop().info().clone(),
                    notes: format!("{} via styx-sensor-bridge, CM5", cfg.mode.mode),
                },
            )
            .map_err(|e| e.to_string())?,
        ),
        None => None,
    };
    if let Some(path) = &a.algo_record {
        let f = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        p.soft_loop()
            .controller()
            .record_to(std::io::BufWriter::new(f))
            .map_err(|e| e.to_string())?;
    }
    let (w, h) = (cfg.mode.width as usize, cfg.mode.height as usize);
    let mut rgb = vec![0u8; w * h * 3];
    let mut frames = Vec::new();
    let (cpu0, _) = process_usage();
    p.start().map_err(|e| e.to_string())?;
    let t_start = Instant::now();
    let mut first = None;
    let mut base = None;
    let mut last_phase = [0.0; 4];
    let mut result = Ok(());
    for i in 0..a.frames as u64 {
        if crate::interrupted() {
            break;
        }
        match controls_for(a, i, base) {
            Some(c) => p.soft_loop().controller().set_controls(c),
            None => p.soft_loop().controller().set_controls(Default::default()),
        }
        let got = p.next(
            TIMEOUT,
            Scale::Full,
            OutputBuffers::Rgb24 {
                data: &mut rgb,
                stride: w * 3,
            },
        );
        let f = match got {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                result = Err(format!("frame {i}: {e}"));
                break;
            }
        };
        let done = monotonic();
        first.get_or_insert(f.raw.dequeued);
        if controls_for(a, i, Some((f.sensor.exposure, f.sensor.analogue_gain))).is_none() {
            base = Some((f.sensor.exposure, f.sensor.analogue_gain));
        }
        let mut log = FrameLog::new(&f.sensor, &f.output.step, f.raw.timestamp);
        log.latency = done.saturating_sub(f.raw.timestamp);
        log.processing = f.raw.dequeued.elapsed();
        log.request_lands = f.request_lands;
        log.out_y = luma_of_rgb(&rgb, w, h);
        if let Some(wr) = &mut writer {
            wr.write(f.raw.data(), &f.sensor, f.raw.timestamp.as_nanos() as u64)
                .map_err(|e| e.to_string())?;
        }
        if i + 1 == a.frames as u64 {
            last_phase = phase_means(f.raw.data(), f.raw.stride as usize, w, h, format.packing);
        }
        if !a.quiet {
            println!("{}", log.line());
        }
        frames.push(log);
    }
    let wall = t_start.elapsed();
    let (cpu1, rss) = process_usage();
    p.soft_loop()
        .controller()
        .stop_recording()
        .map_err(|e| e.to_string())?;
    let stopped = p.close().map_err(|e| e.to_string());
    if let Some(wr) = writer {
        wr.finish().map_err(|e| e.to_string())?;
    }
    result?;
    stopped?;
    let ppm = a.out.join("soft-rgb.ppm");
    write_ppm(&ppm, &rgb, w, h, w * 3).map_err(|e| e.to_string())?;
    write_csv(&a.out.join("soft-frames.csv"), &frames).map_err(|e| e.to_string())?;
    let (rg, bg) = grey_ratios(&rgb, w, h, w * 3, 16, 240);
    let summary = Summary {
        name: "soft (software ISP on the native camera)",
        frames: &frames,
        open_to_first: first.map_or(Duration::ZERO, |f| f - opened),
        cpu: cpu1.saturating_sub(cpu0),
        wall,
        peak_rss: rss,
        extra: vec![
            format!("output grey-world ratios R/G {rg:.3} B/G {bg:.3} (1.000 is neutral)"),
            format!(
                "raw 2x2 cell means (TL, TR, BL, BR) {:.4} {:.4} {:.4} {:.4}",
                last_phase[0], last_phase[1], last_phase[2], last_phase[3]
            ),
            format!("saved {}", ppm.display()),
        ],
    };
    print!("{}", summary.render(a));
    Ok(())
}

pub fn pisp(a: &Args) -> Result<(), String> {
    let tuning = tuning(a)?;
    let (cam, opened) = open_camera(a)?;
    let mut options = PispOptions::nv12_and_half_rgb(1280, 800);
    if a.driver_buffers {
        options.output_memory = styx_pisp::device::OutputMemory::Driver;
    }
    let mut p =
        PispPipeline::open(cam, &settings(a), &tuning, options).map_err(|e| e.to_string())?;
    let (o0, o1) = (
        p.output_format(0).ok_or("no output 0")?,
        p.output_format(1).ok_or("no output 1")?,
    );
    println!(
        "pisp: {:.3} fps; output0 {}x{} stride {} (NV12), output1 {}x{} stride {} (RGB)",
        p.configured().interval.fps(),
        o0.width,
        o0.height,
        o0.stride,
        o1.width,
        o1.height,
        o1.stride
    );
    if let Some(path) = &a.algo_record {
        let f = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        p.controller()
            .record_to(std::io::BufWriter::new(f))
            .map_err(|e| e.to_string())?;
    }
    let (cpu0, _) = process_usage();
    styx_pisp::device::profile::enable(a.profile);
    p.start().map_err(|e| e.to_string())?;
    println!(
        "pisp: frame sync / embedded data: {:?}; back end config buffer: {}",
        p.sensor_feedback(),
        p.back_end_config_source().unwrap_or_default()
    );
    let threads0 = thread_usage();
    let t_start = Instant::now();
    let (w0, h0, s0) = (o0.width as usize, o0.height as usize, o0.stride as usize);
    let (w1, h1, s1) = (o1.width as usize, o1.height as usize, o1.stride as usize);
    let mut frames = Vec::new();
    let mut first = None;
    let mut base = None;
    let mut mismatches = 0;
    let mut last_nv12 = Vec::new();
    let mut last_rgb = Vec::new();
    let mut times: Vec<(PispTimes, Duration)> = Vec::new();
    let mut result = Ok(());
    for i in 0..a.frames as u64 {
        if crate::interrupted() {
            break;
        }
        match controls_for(a, i, base) {
            Some(c) => p.controller().set_controls(c),
            None => p.controller().set_controls(Default::default()),
        }
        let f = match p.next(TIMEOUT) {
            Ok(f) => f,
            Err(e) => {
                result = Err(format!("frame {i}: {e}"));
                break;
            }
        };
        let done = monotonic();
        first.get_or_insert(f.dequeued);
        mismatches += usize::from(f.sequence_mismatch);
        if controls_for(a, i, Some((f.sensor.exposure, f.sensor.analogue_gain))).is_none() {
            base = Some((f.sensor.exposure, f.sensor.analogue_gain));
        }
        let mut log = FrameLog::new(&f.sensor, p.step(), f.timestamp);
        log.latency = done.saturating_sub(f.timestamp);
        log.processing = f.times.total;
        log.request_lands = f.request_lands;
        let tr = Instant::now();
        let last = i + 1 == a.frames as u64;
        if !a.no_read || last {
            p.sync_output(0, &f.job, true).map_err(|e| e.to_string())?;
            if let Some(nv12) = p.output(0, &f.job) {
                if !a.no_read {
                    log.out_y = plane_mean(nv12, w0, h0, s0);
                }
                if last {
                    last_nv12 = nv12.to_vec();
                }
            }
            p.sync_output(0, &f.job, false).map_err(|e| e.to_string())?;
        }
        times.push((f.times, tr.elapsed()));
        if last {
            p.sync_output(1, &f.job, true).map_err(|e| e.to_string())?;
            if let Some(rgb) = p.output(1, &f.job) {
                last_rgb = rgb.to_vec();
            }
            p.sync_output(1, &f.job, false).map_err(|e| e.to_string())?;
        }
        p.release(&f.job);
        if !a.quiet {
            println!("{}", log.line());
        }
        frames.push(log);
    }
    let wall = t_start.elapsed();
    let (cpu1, rss) = process_usage();
    let threads = thread_usage();
    let updates = p.be_updates();
    p.controller().stop_recording().map_err(|e| e.to_string())?;
    let stopped = p.close().map_err(|e| e.to_string());
    result?;
    stopped?;
    let mut extra = vec![
        format!("statistics/raw sequence mismatches: {mismatches}"),
        format!(
            "back end config: rebuilt {}, patched {}, unchanged {} times",
            updates.rebuilt, updates.patched, updates.unchanged
        ),
    ];
    let n = frames.len().max(1) as f64;
    for e in styx_pisp::device::profile::report() {
        extra.push(format!(
            "profile {}.{}: {:.1} us/frame ({:.2} calls/frame, {:.1} us each)",
            e.what,
            e.op,
            e.total.as_secs_f64() * 1e6 / n,
            e.count as f64 / n,
            e.total.as_secs_f64() * 1e6 / e.count.max(1) as f64
        ));
    }
    for t in &threads {
        let before = threads0.iter().find(|b| b.tid == t.tid);
        let t = styx_pipeline::device::ThreadUsage {
            cpu: t
                .cpu
                .saturating_sub(before.map_or(Duration::ZERO, |b| b.cpu)),
            system: t
                .system
                .saturating_sub(before.map_or(Duration::ZERO, |b| b.system)),
            voluntary: t.voluntary - before.map_or(0, |b| b.voluntary),
            involuntary: t.involuntary - before.map_or(0, |b| b.involuntary),
            ..t.clone()
        };
        extra.push(format!(
            "thread {} ({}): {:.3} ms CPU/frame ({:.3} in the kernel), {:.2} waits/frame, {:.2} preemptions/frame",
            t.tid,
            t.name,
            t.cpu.as_secs_f64() * 1e3 / n,
            t.system.as_secs_f64() * 1e3 / n,
            t.voluntary as f64 / n,
            t.involuntary as f64 / n
        ));
    }
    let med = |mut v: Vec<Duration>| {
        v.sort();
        v.get(v.len() / 2)
            .copied()
            .unwrap_or_default()
            .as_secs_f64()
            * 1e3
    };
    let col = |f: fn(&(PispTimes, Duration)) -> Duration| times.iter().map(f).collect::<Vec<_>>();
    let tail = |mut v: Vec<Duration>| {
        v.sort();
        let at = |q: f64| {
            v.get(((v.len().max(1) - 1) as f64 * q).round() as usize)
                .copied()
        };
        let ms = |d: Option<Duration>| d.unwrap_or_default().as_secs_f64() * 1e3;
        format!("p95 {:.3} ms, max {:.3} ms", ms(at(0.95)), ms(at(1.0)))
    };
    extra.push(format!(
        "algorithms {}; dequeue to outputs {}",
        tail(col(|t| t.0.algorithms)),
        tail(col(|t| t.0.total))
    ));
    extra.push(format!(
        "medians: statistics {:.3} ms, algorithms {:.3} ms, back end config + tiles {:.3} ms, back end job {:.3} ms, dequeue to outputs {:.3} ms, output read {:.3} ms",
        med(col(|t| t.0.stats)),
        med(col(|t| t.0.algorithms)),
        med(col(|t| t.0.be_prepare)),
        med(col(|t| t.0.be_job)),
        med(col(|t| t.0.total)),
        med(col(|t| t.1)),
    ));
    if !last_nv12.is_empty() {
        let pgm = a.out.join("pisp-nv12-luma.pgm");
        write_pgm(&pgm, &last_nv12, w0, h0, s0).map_err(|e| e.to_string())?;
        let rgb = nv12_to_rgb(&last_nv12[..s0 * h0], &last_nv12[s0 * h0..], w0, h0, s0);
        let ppm = a.out.join("pisp-nv12-rgb.ppm");
        write_ppm(&ppm, &rgb, w0, h0, w0 * 3).map_err(|e| e.to_string())?;
        let (rg, bg) = grey_ratios(&rgb, w0, h0, w0 * 3, 16, 240);
        extra.push(format!(
            "output0 NV12 grey-world ratios R/G {rg:.3} B/G {bg:.3}; saved {} and {}",
            pgm.display(),
            ppm.display()
        ));
    }
    if !last_rgb.is_empty() {
        let ppm = a.out.join("pisp-rgb-half.ppm");
        write_ppm(&ppm, &last_rgb, w1, h1, s1).map_err(|e| e.to_string())?;
        let (rg, bg) = grey_ratios(&last_rgb, w1, h1, s1, 16, 240);
        extra.push(format!(
            "output1 RGB grey-world ratios R/G {rg:.3} B/G {bg:.3}; saved {}",
            ppm.display()
        ));
    }
    write_csv(&a.out.join("pisp-frames.csv"), &frames).map_err(|e| e.to_string())?;
    let summary = Summary {
        name: "pisp (PiSP front and back end on the native camera)",
        frames: &frames,
        open_to_first: first.map_or(Duration::ZERO, |f| f - opened),
        cpu: cpu1.saturating_sub(cpu0),
        wall,
        peak_rss: rss,
        extra,
    };
    print!("{}", summary.render(a));
    Ok(())
}
