//! `styx-tune capture`: record calibration shots through Styx with fixed exposure and gain, as
//! MCAP recordings (raw frames with the exposure and gains each was made with) named the way
//! `ctt` names its inputs, and a `session.toml` listing them.
//!
//! Without `--step` it walks through a session, saying what to set up before each shot; with
//! `--step` it takes one shot. Bright shots meter themselves first: the exposure (then the
//! gain) moves until the brightest 0.5% of samples sit at `--level` of full scale (0.8), then
//! the frames are recorded with that exposure and gain fixed.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use styx::capture_api::native_controls as ctl;
use styx::prelude::*;
use styx_tune::input::mcap::raw_frame;
use styx_tune::raw::RawFrame;

use crate::{Args, Res};

/// An open camera taking raw frames at settings we choose.
struct Camera {
    device: ProbedDevice,
    handle: CaptureHandle,
    max_exposure_us: f64,
    max_gain: f64,
}

impl Camera {
    fn open(fps: u32) -> Res<Self> {
        let devices = styx::probe_all();
        let (device, mode) = devices
            .iter()
            .filter(|d| d.backends.iter().any(|b| b.kind == BackendKind::Native))
            .find_map(|d| {
                let b = d.backends.iter().find(|b| b.kind == BackendKind::Native)?;
                let mode = b
                    .descriptor
                    .modes
                    .iter()
                    .filter(|m| styx_softisp::bayer_fourcc(m.format.code).is_some())
                    .max_by_key(|m| {
                        let r = m.format.resolution;
                        // Largest, then deepest (RAW10 over RAW8).
                        (
                            r.width.get() * r.height.get(),
                            styx_softisp::bayer_fourcc(m.format.code).map(|(_, p)| p.bit_depth()),
                        )
                    })?;
                Some((d.clone(), mode.clone()))
            })
            .ok_or("no native camera with a raw Bayer mode")?;
        println!(
            "camera: {}, {} {}x{} at {fps} fps",
            device.identity.display,
            mode.format.code,
            mode.format.resolution.width,
            mode.format.resolution.height
        );
        let handle = CaptureRequest::new(&device)
            .backend(BackendKind::Native)
            .mode(mode.id.clone())
            .interval(Interval::from_fps(fps).ok_or("fps")?)
            .control(ctl::EXPOSURE_TIME_US, ControlValue::Uint(10_000))
            .control(ctl::GAIN, ControlValue::Float(1.0))
            .start()?;
        Ok(Self {
            device,
            handle,
            max_exposure_us: 1e6 / f64::from(fps) * 0.97,
            max_gain: 15.5,
        })
    }

    fn set(&self, exposure_us: f64, gain: f64) -> Res<()> {
        self.handle.set_control(
            ctl::EXPOSURE_TIME_US,
            ControlValue::Uint(exposure_us.round().max(1.0) as u32),
        )?;
        self.handle
            .set_control(ctl::GAIN, ControlValue::Float(gain as f32))?;
        Ok(())
    }

    /// `n` frames made with the settings asked for (frames still in flight are skipped),
    /// recorded as they arrive when `rec` is given (frames are not held: the capture has few
    /// buffers). Returns each frame's exposure, gain and mean level, and the last frame.
    fn take(
        &self,
        exposure_us: f64,
        gain: f64,
        n: usize,
        mut rec: Option<&mut StreamRecorder>,
    ) -> Res<(Vec<(f64, f64, f64)>, RawFrame)> {
        self.set(exposure_us, gain)?;
        let deadline =
            Instant::now() + Duration::from_secs(5) + Duration::from_millis(100 * n as u64);
        let mut out: Vec<(f64, f64, f64)> = Vec::new();
        let mut last = None;
        while out.len() < n {
            if Instant::now() > deadline {
                return Err(format!(
                    "frames at {exposure_us:.0} us x {gain:.2} did not arrive (got {})",
                    out.len()
                )
                .into());
            }
            let frame = match self.handle.recv_blocking(Duration::from_secs(2)) {
                RecvOutcome::Data(f) => f,
                RecvOutcome::Empty => continue,
                RecvOutcome::Closed => return Err("capture closed".into()),
            };
            let Some(raw) = raw_frame(&frame) else {
                return Err("not a raw frame".into());
            };
            let exposure_ok =
                (raw.exposure_us - exposure_us).abs() <= (0.04 * exposure_us).max(30.0);
            let gain_ok = (raw.gain() / gain - 1.0).abs() < 0.04;
            let same = out
                .first()
                .is_none_or(|f| f.0 == raw.exposure_us && f.1 == raw.gain());
            if exposure_ok && gain_ok && same {
                if let Some(r) = rec.as_deref_mut() {
                    r.record(&frame)?;
                }
                out.push((
                    raw.exposure_us,
                    raw.gain(),
                    styx_tune::raw::frame_mean(&raw),
                ));
                last = Some(raw);
            }
        }
        Ok((out, last.ok_or("no frames")?))
    }

    /// Settings that put the brightest 0.5% of samples at `level`; the gain stays at
    /// `start_gain` when `fixed_gain` (dimmer frames rather than another gain).
    fn meter(&self, level: f64, start_gain: f64, fixed_gain: bool) -> Res<(f64, f64)> {
        let (mut e, mut g) = (5000.0f64.min(self.max_exposure_us), start_gain);
        for _ in 0..12 {
            let (_, f) = self.take(e, g, 1, None)?;
            let f = &f;
            let mut v: Vec<u16> = f.data.iter().step_by(7).copied().collect();
            let k = (v.len() as f64 * 0.995) as usize;
            let last = v.len() - 1;
            let (_, p, _) = v.select_nth_unstable(k.min(last));
            let black = 64.0 / 1024.0;
            let p = (f64::from(*p) / f.full_scale() - black).max(1e-3);
            let ratio = level / p;
            println!(
                "  metering: {e:.0} us x {g:.2}: brightest at {:.3}",
                p + black
            );
            if (ratio - 1.0).abs() < 0.05 {
                return Ok((e, g));
            }
            // Clipped: the true level is unknown, step down hard.
            let ratio = if p + black > 0.97 { 0.3 } else { ratio };
            let total = e * g * ratio;
            let (e_new, g_new) = if fixed_gain {
                ((total / g).clamp(10.0, self.max_exposure_us), g)
            } else {
                let e_new = total.min(self.max_exposure_us).max(10.0);
                (e_new, (total / e_new).clamp(1.0, self.max_gain))
            };
            if e_new * g_new < total * 0.95 && (fixed_gain || g_new >= self.max_gain) && e_new == e
            {
                println!("  not enough light for level {level}: more light, or a lower --level");
                return Ok((e, g));
            }
            (e, g) = (e_new, g_new);
        }
        Ok((e, g))
    }

    fn recorder(&self, path: &Path) -> Res<StreamRecorder> {
        Ok(StreamRecorder::create(path, &self.device, &self.handle)?)
    }
}

struct Session {
    dir: PathBuf,
    interactive: bool,
}

impl Session {
    fn ask(&self, prompt: &str) -> Res<String> {
        if !self.interactive {
            return Ok(String::new());
        }
        print!("{prompt} ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        Ok(line.trim().to_owned())
    }

    fn append(&self, kind: &str, file: &str, ct: Option<f64>, lux: Option<f64>) -> Res<()> {
        let path = self.dir.join("session.toml");
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(&format!(
            "\n[[shot]]\nkind = \"{kind}\"\nfile = \"{file}\"\n"
        ));
        if let Some(ct) = ct {
            text.push_str(&format!("ct = {ct}\n"));
        }
        if let Some(lux) = lux {
            text.push_str(&format!("lux = {lux}\n"));
        }
        std::fs::write(path, text)?;
        Ok(())
    }
}

fn guidance(step: &str) -> &'static str {
    match step {
        "dark" => "DARK: cover the lens completely (cap, or black cloth over it, room lights low).",
        "black-series" => {
            "BLACK SERIES (no cover needed): a static, dim scene with nothing near clipping; steady light (no flickering lamps)."
        }
        "flat" => {
            "FLAT FIELD: a diffuser (opal glass / several layers of white paper) right against the lens, or an evenly lit white wall filling the frame out of focus; one light of known colour temperature."
        }
        "macbeth" => {
            "COLORCHECKER: the chart square to the camera, filling about half the frame, evenly lit by one light of known colour temperature, no glare. Lux meter at the chart for the lux shot."
        }
        "grey" => {
            "GREY CARD: a neutral grey card filling the centre of the frame, lit by the light of known temperature."
        }
        "noise" => {
            "NOISE: any static scene with a range of brightness (the chart works), steady light."
        }
        _ => "",
    }
}

fn shoot(
    cam: &Camera,
    s: &Session,
    a: &Args,
    step: &str,
    ct: Option<f64>,
    lux: Option<f64>,
) -> Res<()> {
    let frames = a.num("frames")?.map_or(8, |n| n as usize);
    let level = a.num("level")?.unwrap_or(0.8);
    let gain = a.num("gain")?.unwrap_or(1.0);
    let gains: Vec<f64> = a
        .get("gains")
        .unwrap_or("1,2,4,8")
        .split(',')
        .map(|g| g.trim().parse::<f64>())
        .collect::<Result<_, _>>()?;
    let (kind, file) = match step {
        "dark" | "black-series" => {
            let name = if step == "dark" {
                "dark.mcap"
            } else {
                "black_series.mcap"
            };
            let mut rec = cam.recorder(&s.dir.join(name))?;
            for &g in &gains {
                let exposures: Vec<f64> = if step == "dark" {
                    vec![
                        a.num("exposure-us")?
                            .unwrap_or(cam.max_exposure_us.min(30_000.0)),
                    ]
                } else {
                    vec![10.0, 50.0, 200.0, 500.0, 1000.0, 2000.0]
                };
                for e in exposures {
                    let (got, _) = cam.take(e, g, frames, Some(&mut rec))?;
                    let mean = got.iter().map(|f| f.2).sum::<f64>() / got.len() as f64;
                    println!(
                        "  {:.1} us x {:.3}: mean level {:.2} codes (10-bit)",
                        got[0].0,
                        got[0].1,
                        mean * 1024.0
                    );
                }
            }
            rec.finish()?;
            ("dark", name.to_owned())
        }
        _ => {
            let (e, g) = match a.num("exposure-us")? {
                Some(e) => (e, gain),
                None => cam.meter(level, gain, a.get("gain").is_some())?,
            };
            let name = match (step, ct, lux) {
                ("flat", Some(ct), _) => format!("alsc_{ct:.0}k.mcap"),
                ("macbeth", Some(ct), Some(l)) => format!("{ct:.0}k_{l:.0}l.mcap"),
                ("macbeth", Some(ct), None) => format!("{ct:.0}k.mcap"),
                ("grey", Some(ct), _) => format!("grey_{ct:.0}k.mcap"),
                ("noise", _, _) => format!("noise_g{g:.1}.mcap"),
                _ => return Err(format!("--step {step} needs --ct").into()),
            };
            // Never overwrite an earlier shot.
            let stem = name.trim_end_matches(".mcap").to_owned();
            let mut name = name;
            let mut k = 2;
            while s.dir.join(&name).exists() {
                name = format!("{stem}_{k}.mcap");
                k += 1;
            }
            let mut rec = cam.recorder(&s.dir.join(&name))?;
            let (got, _) = cam.take(e, g, frames, Some(&mut rec))?;
            rec.finish()?;
            println!(
                "  recorded {} frames at {:.0} us x {:.3}",
                got.len(),
                got[0].0,
                got[0].1
            );
            (step, name)
        }
    };
    s.append(kind, &file, ct, lux)?;
    println!("  -> {}", s.dir.join(&file).display());
    Ok(())
}

pub fn run(a: &Args) -> Res<()> {
    let dir = PathBuf::from(a.get("out").ok_or("--out DIR")?);
    std::fs::create_dir_all(&dir)?;
    let fps = a.num("fps")?.map_or(30, |f| f as u32);
    let cam = Camera::open(fps)?;
    let s = Session {
        dir,
        interactive: !a.flag("yes"),
    };
    if let Some(step) = a.get("step") {
        println!("{}", guidance(step));
        s.ask("Enter when ready")?;
        return shoot(&cam, &s, a, step, a.num("ct")?, a.num("lux")?);
    }
    if !s.interactive {
        return Err("a guided session needs a terminal; use --step for one shot".into());
    }
    println!(
        "Guided calibration session in {}. Enter to shoot, s to skip a step.",
        s.dir.display()
    );
    for step in ["dark", "flat", "macbeth", "noise"] {
        loop {
            println!("\n{}", guidance(step));
            let ct = if matches!(step, "flat" | "macbeth") {
                let v = s.ask("Colour temperature of the light in K (empty: next step):")?;
                if v.is_empty() {
                    break;
                }
                Some(v.parse::<f64>()?)
            } else {
                None
            };
            let lux = if step == "macbeth" {
                s.ask("Lux at the chart (empty: not measured):")?
                    .parse::<f64>()
                    .ok()
            } else {
                None
            };
            if s.ask("Enter to shoot, s to skip:")? == "s" {
                break;
            }
            if let Err(e) = shoot(&cam, &s, a, step, ct, lux) {
                println!("  failed: {e}");
            }
            if !matches!(step, "flat" | "macbeth") {
                break;
            }
        }
    }
    println!(
        "\nDone. Calibrate with: styx-tune calibrate {}",
        s.dir.display()
    );
    Ok(())
}
