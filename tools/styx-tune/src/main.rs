//! `styx-tune`: calibrate a camera for Styx from raw captures of standard targets.
//!
//! ```text
//! styx-tune calibrate <session.toml | dir> [--out DIR] [--name NAME] [--base TUNING]
//!                     [--grid WxH] [--luminance-strength X] [--target pisp|bcm2835]
//! styx-tune preview <capture> [out.ppm]       find the chart, print its corners, write a picture
//! styx-tune convert <in.json|toml> <out.json|toml>
//! styx-tune synth <dir>                       a synthetic session to try the tool on
//! styx-tune capture --out DIR [...]           record the shots (feature `capture`)
//! ```
//!
//! See `docs/tuning.md`.

#[cfg(feature = "capture")]
mod capture;
mod synth;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use styx_tune::calib::{self, Options};
use styx_tune::input::Loader;
use styx_tune::raw::{Burst, GB, GR};
use styx_tune::session::SessionFile;
use styx_tune::{Tuning, chart};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

/// `--key value` options and positional arguments.
pub struct Args {
    pub positional: Vec<String>,
    pub options: Vec<(String, String)>,
    pub flags: Vec<String>,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>, flags: &[&str]) -> Self {
        let mut a = Self {
            positional: Vec::new(),
            options: Vec::new(),
            flags: Vec::new(),
        };
        let mut it = args.peekable();
        while let Some(s) = it.next() {
            match s.strip_prefix("--") {
                Some(k) if flags.contains(&k) => a.flags.push(k.into()),
                Some(k) => {
                    let v = it.next().unwrap_or_default();
                    a.options.push((k.into(), v));
                }
                None => a.positional.push(s),
            }
        }
        a
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.options
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn num(&self, key: &str) -> Res<Option<f64>> {
        self.get(key)
            .map(|v| {
                v.parse::<f64>()
                    .map_err(|e| format!("--{key} {v}: {e}").into())
            })
            .transpose()
    }

    pub fn flag(&self, key: &str) -> bool {
        self.flags.iter().any(|f| f == key)
    }
}

const USAGE: &str = "usage:
  styx-tune calibrate <session.toml | dir> [--out DIR] [--name NAME] [--base TUNING]
                      [--grid WxH] [--luminance-strength X] [--max-ccm X] [--target pisp|bcm2835]
  styx-tune preview <capture> [out.ppm]
  styx-tune convert <in.json|.toml> <out.json|.toml>
  styx-tune synth <dir>
  styx-tune capture --out DIR [--step dark|black-series|flat|macbeth|grey|noise] [--ct K]
                    [--lux L] [--frames N] [--gain G] [--exposure-us E] [--level X] [--yes]
see docs/tuning.md";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let a = Args::parse(args, &["yes"]);
    let r = match cmd.as_str() {
        "calibrate" => run_calibrate(&a),
        "preview" => run_preview(&a),
        "convert" => run_convert(&a),
        "synth" => synth::run(&a),
        "capture" => run_capture(&a),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("styx-tune: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "capture")]
fn run_capture(a: &Args) -> Res<()> {
    capture::run(a)
}

#[cfg(not(feature = "capture"))]
fn run_capture(_: &Args) -> Res<()> {
    Err("this styx-tune was built without the `capture` feature".into())
}

/// The session and the directory its paths are relative to.
fn session(path: &Path) -> Res<(SessionFile, PathBuf)> {
    if path.is_dir() {
        let s = path.join("session.toml");
        if s.is_file() {
            return Ok((
                SessionFile::parse(&std::fs::read_to_string(&s)?)?,
                path.into(),
            ));
        }
        return Ok((SessionFile::from_dir(path)?, path.into()));
    }
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok((SessionFile::parse(&std::fs::read_to_string(path)?)?, dir))
}

fn run_calibrate(a: &Args) -> Res<()> {
    let input = a.positional.first().ok_or(USAGE)?;
    let (session, dir) = session(Path::new(input))?;
    let mut opts = Options {
        description: session.description.clone(),
        ..Options::default()
    };
    if let Some(g) = a.get("grid") {
        let (w, h) = g.split_once('x').ok_or("--grid WxH")?;
        opts.grid = (w.parse()?, h.parse()?);
    }
    if let Some(v) = a.num("luminance-strength")? {
        opts.luminance_strength = v;
    }
    if let Some(v) = a.num("max-ccm")? {
        opts.max_ccm_coefficient = v;
    }
    let mut base = match a.get("base") {
        Some(p) => Tuning::load(p)?,
        None => Tuning::from_toml_str(styx_pipeline::tuning::GENERIC_TUNING)?,
    };
    // Complete files: libcamera needs every section.
    base.agc.get_or_insert_with(Default::default);
    base.contrast.get_or_insert_with(Default::default);
    let shots = session.load(&dir, &Loader::new())?;
    let frames: usize = shots.iter().map(|s| s.frames.len()).sum();
    eprintln!("{} shots, {frames} frames", shots.len());
    let cal = calib::calibrate(&shots, &base, &opts)?;
    let report = styx_tune::report::text(&cal);
    print!("{report}");
    let name = a
        .get("name")
        .map(str::to_owned)
        .or(session.sensor.clone())
        .unwrap_or_else(|| "sensor".into());
    let out = PathBuf::from(a.get("out").unwrap_or("."));
    std::fs::create_dir_all(&out)?;
    let toml = out.join(format!("{name}.toml"));
    let json = out.join(format!("{name}.json"));
    std::fs::write(&toml, cal.tuning.to_toml_string()?)?;
    std::fs::write(&json, cal.tuning.to_rpi_json_string(a.get("target")))?;
    std::fs::write(out.join(format!("{name}-report.txt")), &report)?;
    eprintln!("wrote {} and {}", toml.display(), json.display());
    Ok(())
}

fn run_convert(a: &Args) -> Res<()> {
    let [input, output] = a.positional.get(..2).ok_or(USAGE)? else {
        return Err(USAGE.into());
    };
    let t = Tuning::load(input)?;
    let text = if output.ends_with(".json") {
        t.to_rpi_json_string(a.get("target"))
    } else {
        t.to_toml_string()?
    };
    std::fs::write(output, text)?;
    Ok(())
}

/// Find the chart in a capture, print where, and write a picture with the patches marked.
fn run_preview(a: &Args) -> Res<()> {
    let input = a.positional.first().ok_or(USAGE)?;
    let frames = Loader::new().load(Path::new(input))?;
    let f = &frames[frames.len() / 2..];
    let black = f[0].black_level.unwrap_or(4096.0 / 65536.0);
    let burst = Burst::new(f, black).ok_or("frames of different sizes")?;
    let mut planes = burst.mean.clone();
    planes.subtract([black; 4]);
    println!(
        "{} frames, {}x{}, {:.0} us x {:.2}",
        f.len(),
        burst.size.0,
        burst.size.1,
        burst.exposure_us,
        burst.gain()
    );
    let found = chart::detect(&planes);
    match &found {
        Some(d) => {
            println!(
                "chart found ({} patches seen); corner patch centres, full-resolution pixels:",
                d.found
            );
            for (name, i) in [
                ("dark skin", 0),
                ("bluish green", 5),
                ("black", 23),
                ("white", 18),
            ] {
                let [x, y] = d.chart.centre_full(i);
                println!("  {name:>12}: [{x:.0}, {y:.0}]");
            }
        }
        None => println!("no chart found"),
    }
    let out = a
        .positional
        .get(1)
        .cloned()
        .unwrap_or_else(|| format!("{}.ppm", Path::new(input).with_extension("").display()));
    write_ppm(&planes, found.as_ref().map(|d| &d.chart), Path::new(&out))?;
    println!("wrote {out}");
    Ok(())
}

/// Half-resolution picture: grey-world balanced, sRGB-encoded, patch centres marked.
fn write_ppm(p: &styx_tune::raw::Planes, chart: Option<&chart::Chart>, out: &Path) -> Res<()> {
    let m = p.mean();
    let g = (m[GR] + m[GB]) / 2.0;
    let gains = [g / m[0].max(1e-6), 1.0, g / m[3].max(1e-6)];
    let mut peak = 0f64;
    for i in 0..p.width * p.height {
        peak = peak.max(f64::from(p.ch[1][i]));
    }
    let scale = 1.0 / (peak.max(1e-6) * 1.05);
    let mut img = vec![0u8; p.width * p.height * 3];
    for y in 0..p.height {
        for x in 0..p.width {
            let rgb = [
                f64::from(p.at(0, x, y)) * gains[0],
                f64::from(p.at(1, x, y) + p.at(2, x, y)) / 2.0,
                f64::from(p.at(3, x, y)) * gains[2],
            ];
            for c in 0..3 {
                let v = styx_tune::colour::linear_to_srgb((rgb[c] * scale).clamp(0.0, 1.0));
                img[(y * p.width + x) * 3 + c] = (v * 255.0).round() as u8;
            }
        }
    }
    if let Some(c) = chart {
        for i in 0..24 {
            let [cx, cy] = c.centre(i);
            let h = (0.2 * c.pitch(i)).max(2.0) as i64;
            for d in -h..=h {
                for (x, y) in [(cx as i64 + d, cy as i64), (cx as i64, cy as i64 + d)] {
                    if (0..p.width as i64).contains(&x) && (0..p.height as i64).contains(&y) {
                        let k = (y as usize * p.width + x as usize) * 3;
                        img[k..k + 3].copy_from_slice(if i == 0 {
                            &[255, 0, 0]
                        } else {
                            &[255, 0, 255]
                        });
                    }
                }
            }
        }
    }
    let mut data = format!("P6\n{} {}\n255\n", p.width, p.height).into_bytes();
    data.extend(img);
    std::fs::write(out, data)?;
    Ok(())
}
