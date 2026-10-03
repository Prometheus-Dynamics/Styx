//! The GPU ISP (feature `gpu`) on a raw recording:
//!
//! * `gpu-quality`: the 3A loop runs over the recording with the software ISP; every frame is
//!   also processed on the GPU with the settings the loop applied to it, and the pictures
//!   (RGB24, NV12) and statistics are compared with the software ISP's integer arithmetic
//!   (the GPU's: expected identical), its tone quadratics (x86's default) and fp16 (the
//!   Cortex-A76's default).
//! * `gpu-bench`: the loop on the CPU (`--threads`) and on the GPU, frames paced at `--fps`:
//!   wall time of the ISP, process CPU time (all threads, the Vulkan driver's included) and
//!   GPU time (timestamp queries) per frame.

use std::time::{Duration, Instant};

use styx_pipeline::rawrec::RawRecording;
use styx_pipeline::replay::VirtualSensor;
use styx_pipeline::styx_gpuisp::{DeviceSelect, GpuContext, GpuIsp};
use styx_pipeline::{IspEngine, SensorInfo, SoftLoop, soft::base_params};
use styx_softisp::{Arithmetic, IspParams, IspStats, OutputBuffers, Scale, SoftIsp};

use crate::output::{Kind, Output};
use crate::{Args, tuning};

/// Differences of one channel accumulated over frames.
#[derive(Default, Clone, Copy)]
struct Diff {
    sq: f64,
    n: u64,
    max: u8,
    over2: u64,
}

impl Diff {
    fn add(&mut self, a: u8, b: u8) {
        let d = a.abs_diff(b);
        self.sq += f64::from(d) * f64::from(d);
        self.n += 1;
        self.max = self.max.max(d);
        self.over2 += u64::from(d > 2);
    }

    fn line(&self, name: &str) -> String {
        let mse = self.sq / self.n.max(1) as f64;
        let psnr = if mse == 0.0 {
            "identical".to_string()
        } else {
            format!("{:.2} dB", 10.0 * (255.0 * 255.0 / mse).log10())
        };
        format!(
            "    {name:8} PSNR {psnr:>10}, max {} codes, {:.4}% more than 2 apart",
            self.max,
            100.0 * self.over2 as f64 / self.n.max(1) as f64
        )
    }
}

/// Largest relative difference of the zone sums (zones with 16 quads or more) and of the
/// histogram counts.
fn stats_diff(a: &IspStats, b: &IspStats, worst: &mut (f64, f64)) {
    for (x, y) in a.zones.iter().zip(&b.zones) {
        if x.count < 16 {
            continue;
        }
        for (p, q) in [(x.r_sum, y.r_sum), (x.g_sum, y.g_sum), (x.b_sum, y.b_sum)] {
            worst.0 = worst.0.max(p.abs_diff(q) as f64 / p.max(1) as f64);
        }
    }
    let total: u64 = a.histogram.iter().map(|&c| c as u64).sum();
    let moved: u64 = a
        .histogram
        .iter()
        .zip(&b.histogram)
        .map(|(&p, &q)| p.abs_diff(q) as u64)
        .sum();
    worst.1 = worst.1.max(moved as f64 / 2.0 / total.max(1) as f64);
}

fn context() -> Result<GpuContext, String> {
    let ctx = GpuContext::open(DeviceSelect::Auto).map_err(|e| e.to_string())?;
    let i = ctx.info();
    println!(
        "GPU: {} ({:?}, {}), dma-buf {}",
        i.name, i.kind, i.driver, i.dmabuf
    );
    Ok(ctx)
}

fn open(a: &Args, name: &str) -> Result<RawRecording, String> {
    let base = a
        .recording
        .as_ref()
        .ok_or(format!("{name} needs --recording BASE"))?;
    RawRecording::open(base).map_err(|e| format!("{}: {e}", base.display()))
}

/// RGB24, NV12 and the statistics of one frame.
type Pictures = (Vec<u8>, Vec<u8>, Option<IspStats>);

fn rgb_nv12(
    mut f: impl FnMut(OutputBuffers<'_>) -> Result<Option<IspStats>, String>,
    w: usize,
    h: usize,
) -> Result<Pictures, String> {
    let mut rgb = vec![0u8; w * h * 3];
    let s = f(OutputBuffers::Rgb24 {
        data: &mut rgb,
        stride: w * 3,
    })?;
    let mut nv = vec![0u8; w * h * 3 / 2];
    let (y, uv) = nv.split_at_mut(w * h);
    f(OutputBuffers::Nv12 {
        y,
        y_stride: w,
        uv,
        uv_stride: w,
    })?;
    Ok((rgb, nv, s))
}

pub fn quality(a: &Args) -> Result<(), String> {
    let rec = open(a, "gpu-quality")?;
    let info: SensorInfo = rec
        .header
        .sensor
        .clone()
        .with_fps(a.fps, a.fps)
        .map_err(|e| e.to_string())?;
    let bits = info.bits;
    let ctx = context()?;
    let mut sensor = VirtualSensor::new(&rec);
    let format = sensor.format();
    let mut soft =
        SoftLoop::new(info, format.packing, &tuning(a)?, 1).map_err(|e| e.to_string())?;
    soft.set_settled_rate(None);
    let start = soft.start().map_err(|e| e.to_string())?;
    if let Some(r) = start.sensor {
        sensor.request(&r);
    }
    let (w, h) = (format.width as usize, format.height as usize);
    let with = |arithmetic| IspParams {
        arithmetic,
        ..base_params()
    };
    let mut gpu =
        GpuIsp::with_context(&ctx, format, with(Arithmetic::Int)).map_err(|e| e.to_string())?;
    let refs = [Arithmetic::Int, Arithmetic::IntPolyTone, Arithmetic::Half];
    let mut cpus: Vec<SoftIsp> = refs
        .iter()
        .map(|&r| SoftIsp::new(format, with(r)).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    let mut diffs = [[Diff::default(); 5]; 3];
    let mut worst = [(0.0f64, 0.0f64); 3];
    let mut identical_stats = [0usize; 3];
    let mut frames = 0;
    let mut skipped = 0;
    for _ in 0..a.frames.min(rec.len()) {
        let stride = sensor.stride();
        let (raw, values) = sensor.next_frame();
        let raw = raw.to_vec();
        let mut scratch = Output::new(Kind::Rgb, Scale::Full, w, h);
        let out = soft
            .process(&raw, stride, &values, Scale::Full, scratch.buffers())
            .map_err(|e| e.to_string())?;
        if let Some(r) = &out.step.sensor {
            sensor.request(r);
        }
        let applied = out.applied.softisp(bits, &with(Arithmetic::Int));
        gpu.set_params(applied.clone()).map_err(|e| e.to_string())?;
        let g = rgb_nv12(
            |o| {
                gpu.process(&raw, stride, Scale::Full, o)
                    .map_err(|e| e.to_string())
            },
            w,
            h,
        )?;
        // The integer arithmetic (so the GPU) limits white balance x digital gain x lens
        // shading to 16 (Q12); fp16 applies larger gains. Those frames are not compared
        // with fp16 (as `quality` leaves them out).
        let lsc_max = applied.lens_shading.as_ref().map_or(1.0, |l| {
            [&l.r, &l.g, &l.b]
                .iter()
                .flat_map(|t| t.iter())
                .fold(0.0f32, |m, &v| m.max(v))
        });
        let wb = applied.white_balance.map_or([1.0; 3], |w| w.gains());
        let range = 1023.0 / (1023.0 - 64.0);
        let beyond =
            wb.iter().fold(0.0f32, |m, &g| m.max(g)) * applied.digital_gain * lsc_max * range
                > 16.0;
        skipped += usize::from(beyond);
        for (k, cpu) in cpus.iter_mut().enumerate() {
            if beyond && refs[k] == Arithmetic::Half {
                continue;
            }
            cpu.set_params(IspParams {
                arithmetic: refs[k],
                ..applied.clone()
            })
            .map_err(|e| e.to_string())?;
            let c = rgb_nv12(
                |o| {
                    cpu.process(&raw, stride, Scale::Full, o)
                        .map_err(|e| e.to_string())
                },
                w,
                h,
            )?;
            for (p, q) in c.0.chunks_exact(3).zip(g.0.chunks_exact(3)) {
                for ch in 0..3 {
                    diffs[k][ch].add(p[ch], q[ch]);
                }
            }
            c.1[..w * h]
                .iter()
                .zip(&g.1[..w * h])
                .for_each(|(&p, &q)| diffs[k][3].add(p, q));
            c.1[w * h..]
                .iter()
                .zip(&g.1[w * h..])
                .for_each(|(&p, &q)| diffs[k][4].add(p, q));
            if let (Some(cs), Some(gs)) = (&c.2, &g.2) {
                stats_diff(cs, gs, &mut worst[k]);
                identical_stats[k] += usize::from(cs == gs);
            }
        }
        frames += 1;
    }
    println!(
        "GPU ISP against the software ISP, {frames} frames of {} with the loop's settings:",
        a.recording.as_ref().expect("checked").display()
    );
    for (k, r) in refs.iter().enumerate() {
        if *r == Arithmetic::Half {
            println!("  {r:?} ({skipped} frames left out: gains above the integer path's 16):");
        } else {
            println!("  {r:?}:");
        }
        for (d, name) in diffs[k].iter().zip(["R", "G", "B", "NV12 Y", "NV12 UV"]) {
            println!("{}", d.line(name));
        }
        println!(
            "    statistics: identical on {}/{frames} frames; zone sums at most {:.4}% apart, \
             at most {:.4}% of the histogram in another bin",
            identical_stats[k],
            100.0 * worst[k].0,
            100.0 * worst[k].1
        );
    }
    Ok(())
}

/// Process CPU time (all threads), from `getrusage`.
fn cpu_time() -> Duration {
    // SAFETY: plain query into a zeroed struct.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointer.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(u.ru_utime) + tv(u.ru_stime)
}

pub fn bench(a: &Args) -> Result<(), String> {
    let rec = open(a, "gpu-bench")?;
    let info: SensorInfo = rec
        .header
        .sensor
        .clone()
        .with_fps(a.fps, a.fps)
        .map_err(|e| e.to_string())?;
    let ctx = context()?;
    let mut runs: Vec<Option<usize>> = vec![Some(a.threads)];
    if a.threads != 1 {
        runs.insert(0, Some(1));
    }
    runs.push(None);
    let period = Duration::from_secs_f64(1.0 / a.fps);
    println!(
        "{} frames at {} fps, {:?} {:?}, statistics and algorithms {}:",
        a.frames,
        a.fps,
        a.output.0,
        a.output.1,
        if a.every_frame {
            "on every frame"
        } else {
            "at 15 Hz once settled"
        }
    );
    for run in runs {
        let mut sensor = VirtualSensor::new(&rec);
        let format = sensor.format();
        let mut soft = SoftLoop::new(info.clone(), format.packing, &tuning(a)?, run.unwrap_or(1))
            .map_err(|e| e.to_string())?;
        soft.set_base_params(crate::soft_base(a));
        if a.every_frame {
            soft.set_settled_rate(None);
        }
        if run.is_none() {
            soft.use_gpu(&ctx).map_err(|e| e.to_string())?;
        }
        let start = soft.start().map_err(|e| e.to_string())?;
        if let Some(r) = start.sensor {
            sensor.request(&r);
        }
        let mut out = Output::new(
            a.output.0,
            a.output.1,
            format.width as usize,
            format.height as usize,
        );
        let (mut cpu, mut wall, mut isp, mut gpu, mut settings) = (
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        );
        let (mut n, mut gpu_n) = (0u32, 0u32);
        let warmup = 10;
        let mut next = Instant::now();
        for i in 0..a.frames + warmup {
            let stride = sensor.stride();
            let (raw, values) = sensor.next_frame();
            let raw = raw.to_vec();
            // Paced as a camera delivers frames.
            next += period;
            if let Some(d) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
            let (c0, t0) = (cpu_time(), Instant::now());
            let o = soft
                .process(&raw, stride, &values, out.scale, out.buffers())
                .map_err(|e| e.to_string())?;
            let (t1, c1) = (Instant::now(), cpu_time());
            if let Some(r) = &o.step.sensor {
                sensor.request(r);
            }
            if i < warmup {
                continue;
            }
            n += 1;
            cpu += c1 - c0;
            wall += t1 - t0;
            isp += o.timing.isp;
            settings += o.timing.settings;
            if let Some(g) = soft.gpu_time() {
                gpu += g;
                gpu_n += 1;
            }
        }
        let ms = |d: Duration, n: u32| d.as_secs_f64() * 1e3 / f64::from(n.max(1));
        let name = match soft.engine() {
            IspEngine::Cpu { threads } => {
                format!("CPU, {threads} thread(s), {:?}", soft.arithmetic())
            }
            IspEngine::Gpu { device } => format!("GPU, {device}"),
        };
        println!(
            "  {name}: process CPU {:.3} ms/frame ({:.1}% of a core), loop wall {:.3} ms \
             (ISP {:.3}, settings {:.3}){}",
            ms(cpu, n),
            100.0 * cpu.as_secs_f64() / (period.as_secs_f64() * f64::from(n.max(1))),
            ms(wall, n),
            ms(isp, n),
            ms(settings, n),
            if gpu_n > 0 {
                format!(", GPU {:.3} ms", ms(gpu, gpu_n))
            } else {
                String::new()
            }
        );
    }
    Ok(())
}
