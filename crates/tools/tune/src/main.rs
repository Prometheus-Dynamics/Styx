//! `styx-tune`: calibrate a camera for Styx from raw captures of standard targets, and make a
//! synthetic session to try it on.
//!
//! ```text
//! styx-tune calibrate <session.toml> [--name NAME] [--out DIR] [--base FILE]
//!                     [--grid WxH] [--description TEXT]
//! styx-tune synth <DIR>
//! ```
//!
//! `calibrate` writes `<DIR>/<NAME>.toml`, where Styx's tuning search finds it. DIR defaults to
//! the first added tuning directory (`STYX_TUNING_PATH`, else `/etc/styx/tuning`); the bundled
//! install directory is refused. NAME defaults to the session's `sensor`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use styx_tune::calib::{self, Options};
use styx_tune::session::SessionFile;
use styx_tune::synth::{SensorModel, session};
use styx_tune::{Tuning, output, report};

const USAGE: &str = "usage:
  styx-tune calibrate <session.toml> [--name NAME] [--out DIR] [--base FILE] [--grid WxH] [--description TEXT]
  styx-tune synth <DIR>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("styx-tune: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("calibrate") => calibrate(&args[1..]),
        Some("synth") => {
            let dir = args.get(1).ok_or(USAGE)?;
            let shots = session::standard_session(&SensorModel::default()).0;
            let path = session::write_session(Path::new(dir), "synthetic", &shots)
                .map_err(|e| e.to_string())?;
            println!("wrote {}", path.display());
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

fn calibrate(args: &[String]) -> Result<(), String> {
    let mut session_path: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut base: Option<PathBuf> = None;
    let mut opts = Options::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--name" => name = Some(value("--name")?),
            "--out" => out = Some(PathBuf::from(value("--out")?)),
            "--base" => base = Some(PathBuf::from(value("--base")?)),
            "--description" => opts.description = Some(value("--description")?),
            "--grid" => {
                let v = value("--grid")?;
                let (w, h) = v
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                    .ok_or("--grid takes WxH, e.g. 16x12")?;
                opts.grid = (w, h);
            }
            other if other.starts_with("--") => return Err(format!("unknown option {other}")),
            other => {
                if session_path.replace(PathBuf::from(other)).is_some() {
                    return Err("one session file".into());
                }
            }
        }
    }
    let session_path = session_path.ok_or(USAGE)?;
    let session = SessionFile::read(&session_path).map_err(|e| e.to_string())?;
    let dir = session_path.parent().unwrap_or(Path::new("."));
    let shots = session.load(dir).map_err(|e| e.to_string())?;
    let base = match base {
        Some(p) => {
            let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            toml::from_str::<Tuning>(&text).map_err(|e| format!("{}: {e}", p.display()))?
        }
        None => Tuning::default(),
    };
    if opts.description.is_none() {
        opts.description = session.description.clone();
    }
    let name = name
        .or_else(|| session.sensor.clone())
        .ok_or("no tuning name: give --name or set `sensor` in the session")?;
    let out = match out {
        Some(d) => d,
        None => styx_tuning::TuningSearch::system()
            .added
            .into_iter()
            .next()
            .ok_or("no added tuning directory; give --out")?,
    };
    let cal = calib::calibrate(&shots, &base, &opts).map_err(|e| e.to_string())?;
    let path = output::write(&out, &name, &cal.tuning).map_err(|e| e.to_string())?;
    print!("{}", report::text(&cal));
    println!("wrote {}", path.display());
    Ok(())
}
