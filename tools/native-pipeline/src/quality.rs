//! `quality`: the software ISP's two arithmetics on the same frames. The 3A loop runs over a
//! raw recording with the integer arithmetic (the reference); every frame is also processed
//! with the settings the loop applied to it in fp16, and the pictures (RGB24 and NV12) and
//! statistics are compared: PSNR and largest difference per channel, how many samples differ
//! by more than one code, and the zone sums.

use styx_pipeline::rawrec::RawRecording;
use styx_pipeline::replay::VirtualSensor;
use styx_pipeline::{SensorInfo, SoftLoop, soft::base_params};
use styx_softisp::{Arithmetic, IspStats, OutputBuffers, Scale, SoftIsp};

use crate::{Args, tuning};

/// Differences of one channel (or plane) accumulated over frames.
#[derive(Default, Clone, Copy)]
struct Diff {
    sq: f64,
    n: u64,
    max: u8,
    over1: u64,
}

impl Diff {
    fn add(&mut self, a: u8, b: u8) {
        let d = a.abs_diff(b);
        self.sq += f64::from(d) * f64::from(d);
        self.n += 1;
        self.max = self.max.max(d);
        self.over1 += u64::from(d > 1);
    }

    fn psnr(&self) -> f64 {
        let mse = self.sq / self.n.max(1) as f64;
        if mse == 0.0 {
            f64::INFINITY
        } else {
            10.0 * (255.0 * 255.0 / mse).log10()
        }
    }

    fn line(&self, name: &str) -> String {
        format!(
            "  {name:6} PSNR {:6.2} dB, max {} codes, {:.4}% of samples off by more than 1",
            self.psnr(),
            self.max,
            100.0 * self.over1 as f64 / self.n.max(1) as f64
        )
    }
}

/// Largest relative difference of the zone sums (zones with at least `min` quads counted).
fn stats_diff(a: &IspStats, b: &IspStats, worst: &mut f64) {
    for (x, y) in a.zones.iter().zip(&b.zones) {
        if x.count < 16 {
            continue;
        }
        for (p, q) in [(x.r_sum, y.r_sum), (x.g_sum, y.g_sum), (x.b_sum, y.b_sum)] {
            let d = p.abs_diff(q) as f64 / (p.max(1) as f64);
            *worst = worst.max(d);
        }
    }
}

pub fn run(a: &Args) -> Result<(), String> {
    let base = a
        .recording
        .as_ref()
        .ok_or("quality needs --recording BASE")?;
    let rec = RawRecording::open(base).map_err(|e| format!("{}: {e}", base.display()))?;
    let h = &rec.header;
    let info: SensorInfo = h
        .sensor
        .clone()
        .with_fps(a.fps, a.fps)
        .map_err(|e| e.to_string())?;
    let bits = info.bits;
    let mut sensor = VirtualSensor::new(&rec);
    let format = sensor.format();
    let mut soft =
        SoftLoop::new(info, format.packing, &tuning(a)?, 1).map_err(|e| e.to_string())?;
    let fixed = |arithmetic| styx_softisp::IspParams {
        arithmetic,
        ..base_params()
    };
    soft.set_base_params(fixed(Arithmetic::Int));
    let mut half =
        SoftIsp::new(format, fixed(Arithmetic::Half).clone()).map_err(|e| e.to_string())?;
    let start = soft.start().map_err(|e| e.to_string())?;
    if let Some(r) = start.sensor {
        sensor.request(&r);
    }
    let (w, ht) = (format.width as usize, format.height as usize);
    let (mut rgb_i, mut rgb_h) = (vec![0u8; w * ht * 3], vec![0u8; w * ht * 3]);
    let mut rgb = [Diff::default(); 3];
    let (mut y_d, mut uv_d) = (Diff::default(), Diff::default());
    let mut worst_stats = 0.0f64;
    let mut skipped = 0;
    for _ in 0..a.frames.min(rec.len()) {
        let stride = sensor.stride();
        let (raw, values) = sensor.next_frame();
        let raw = raw.to_vec();
        let out = soft
            .process(
                &raw,
                stride,
                &values,
                Scale::Full,
                OutputBuffers::Rgb24 {
                    data: &mut rgb_i,
                    stride: w * 3,
                },
            )
            .map_err(|e| e.to_string())?;
        if let Some(r) = &out.step.sensor {
            sensor.request(r);
        }
        let applied = out.applied.softisp(bits, &fixed(Arithmetic::Half));
        // The integer arithmetic limits white balance x digital gain x lens shading to 16
        // (Q12); fp16 applies larger gains. Frames that need them are not compared.
        let lsc_max = applied.lens_shading.as_ref().map_or(1.0, |l| {
            [&l.r, &l.g, &l.b]
                .iter()
                .flat_map(|t| t.iter())
                .fold(0.0f32, |m, &v| m.max(v))
        });
        let wb = applied.white_balance.map_or([1.0; 3], |w| w.gains());
        let range = 1023.0 / (1023.0 - 64.0);
        if wb.iter().fold(0.0f32, |m, &g| m.max(g)) * applied.digital_gain * lsc_max * range > 16.0
        {
            skipped += 1;
            continue;
        }
        half.set_params(applied.clone())
            .map_err(|e| e.to_string())?;
        if half.arithmetic() != Arithmetic::Half {
            return Err("these settings do not run in fp16".into());
        }
        let hs = half
            .process(
                &raw,
                stride,
                Scale::Full,
                OutputBuffers::Rgb24 {
                    data: &mut rgb_h,
                    stride: w * 3,
                },
            )
            .map_err(|e| e.to_string())?;
        for (p, q) in rgb_i.chunks_exact(3).zip(rgb_h.chunks_exact(3)) {
            for c in 0..3 {
                rgb[c].add(p[c], q[c]);
            }
        }
        // NV12 of both, and the statistics of the integer arithmetic on the same settings.
        let mut int = SoftIsp::new(
            format,
            styx_softisp::IspParams {
                arithmetic: Arithmetic::Int,
                ..applied
            },
        )
        .map_err(|e| e.to_string())?;
        let nv = |isp: &mut SoftIsp| {
            let (mut y, mut uv) = (vec![0u8; w * ht], vec![0u8; w * ht / 2]);
            let s = isp.process(
                &raw,
                stride,
                Scale::Full,
                OutputBuffers::Nv12 {
                    y: &mut y,
                    y_stride: w,
                    uv: &mut uv,
                    uv_stride: w,
                },
            );
            s.map(|s| (y, uv, s))
        };
        let (yi, uvi, si) = nv(&mut int).map_err(|e| e.to_string())?;
        let (yh, uvh, _) = nv(&mut half).map_err(|e| e.to_string())?;
        yi.iter().zip(&yh).for_each(|(&p, &q)| y_d.add(p, q));
        uvi.iter().zip(&uvh).for_each(|(&p, &q)| uv_d.add(p, q));
        if let (Some(si), Some(sh)) = (si, hs) {
            stats_diff(&si, &sh, &mut worst_stats);
        }
    }
    let save = |name: &str, rgb: &[u8]| {
        styx_pipeline::measure::write_ppm(&a.out.join(name), rgb, w, ht, w * 3)
            .map_err(|e| e.to_string())
    };
    save("quality-int.ppm", &rgb_i)?;
    save("quality-half.ppm", &rgb_h)?;
    println!(
        "fp16 against the integer arithmetic, {} frames of {} with the loop's settings \
         ({skipped} more left out: gains above the integer path's limit of 16):",
        a.frames.min(rec.len()) - skipped,
        base.display()
    );
    for (c, name) in ["R", "G", "B"].iter().enumerate() {
        println!("{}", rgb[c].line(name));
    }
    println!("{}", y_d.line("NV12 Y"));
    println!("{}", uv_d.line("NV12 UV"));
    println!(
        "  statistics: zone sums at most {:.3}% apart",
        100.0 * worst_stats
    );
    Ok(())
}
