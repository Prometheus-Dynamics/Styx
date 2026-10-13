//! styx-calibrate: the command line over the calibration library.
//!
//! ```text
//! styx-calibrate dark DIR... [--out DIR] [--sensor NAME] [--unit ID] [--cfa BGGR]
//!                [--size WxH] [--bits N] [--hot-k K] [--black-move LSB]
//!                [--styx-commit HASH] [--date YYYY-MM-DD]
//!                [--per-channel-black] [--noise-model]
//! styx-calibrate check-processed DIR... [--out FILE]
//! styx-calibrate help
//! ```
//!
//! `dark` writes `DIR/calibration.md` (the report) and `DIR/<sensor>.overlay.json` (the tuning
//! overlay); `--out` names the directory (default `styx-calibration`). `check-processed` prints
//! its report to stdout, or to `--out`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use styx_calibrate::raw::err;
use styx_calibrate::{RunOptions, processed, report, run_dark};

const USAGE: &str = "usage:
  styx-calibrate dark DIR... [--out DIR] [--sensor NAME] [--unit ID] [--cfa BGGR]
                     [--size WxH] [--bits N] [--hot-k K] [--black-move LSB]
                     [--styx-commit HASH] [--date YYYY-MM-DD]
                     [--per-channel-black] [--noise-model]
  styx-calibrate check-processed DIR... [--out FILE]

A dark DIR holds one raw dump (frames_<W>x<H>_bggr16.raw) with frames.csv and meta.txt, taken
at one exposure and gain. See docs/calibration.md.";

/// Options, and the directories given.
#[derive(Default)]
struct Args {
    dirs: Vec<PathBuf>,
    out: Option<PathBuf>,
    run: RunOptions,
}

fn parse(args: &[String]) -> styx_calibrate::Result<Args> {
    let mut a = Args::default();
    let mut i = 0;
    let value = |i: &mut usize| -> styx_calibrate::Result<String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| styx_calibrate::Error(format!("{} needs a value", args[*i - 1])))
    };
    let number = |s: String, flag: &str| -> styx_calibrate::Result<f64> {
        s.parse()
            .map_err(|_| styx_calibrate::Error(format!("{flag} needs a number (got {s})")))
    };
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--out" => a.out = Some(PathBuf::from(value(&mut i)?)),
            "--sensor" => a.run.sensor = Some(value(&mut i)?),
            "--unit" => a.run.unit = Some(value(&mut i)?),
            "--cfa" => a.run.raw.cfa = Some(value(&mut i)?),
            "--date" => a.run.date = Some(value(&mut i)?),
            "--styx-commit" => a.run.styx_commit = Some(value(&mut i)?),
            "--per-channel-black" => a.run.overlay.per_channel_black = true,
            "--noise-model" => a.run.overlay.noise_model = true,
            "--size" => {
                let v = value(&mut i)?;
                let (w, h) = v
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                    .ok_or_else(|| styx_calibrate::Error(format!("--size needs WxH (got {v})")))?;
                a.run.raw.size = Some((w, h));
            }
            "--bits" => a.run.raw.bits = Some(number(value(&mut i)?, arg)? as u32),
            "--hot-k" => a.run.dark.hot_k = number(value(&mut i)?, arg)?,
            "--black-move" => a.run.dark.black_move = number(value(&mut i)?, arg)?,
            _ if arg.starts_with("--") => {
                return err(format!("unknown option {arg}\n{USAGE}"));
            }
            _ => a.dirs.push(PathBuf::from(arg)),
        }
        i += 1;
    }
    Ok(a)
}

/// The Styx commit the tool runs from, `unknown` outside a checkout.
fn git_commit() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn write(path: &Path, text: &str) -> styx_calibrate::Result<()> {
    std::fs::write(path, text)
        .map_err(|e| styx_calibrate::Error(format!("{}: {e}", path.display())))
}

fn run_dark_command(mut a: Args) -> styx_calibrate::Result<()> {
    if a.dirs.is_empty() {
        return err(format!(
            "dark needs at least one capture directory\n{USAGE}"
        ));
    }
    if a.run.styx_commit.is_none() {
        a.run.styx_commit = git_commit();
    }
    let run = run_dark(&a.dirs, &a.run)?;
    let out = a.out.unwrap_or_else(|| PathBuf::from("styx-calibration"));
    std::fs::create_dir_all(&out)?;
    let report_path = out.join("calibration.md");
    let overlay_path = out.join(format!("{}.overlay.json", run.sensor));
    write(&report_path, &run.report)?;
    write(&overlay_path, &run.overlay_text())?;
    println!("report:  {}", report_path.display());
    println!("overlay: {}", overlay_path.display());
    println!(
        "{} captures, {} gains, {} defect pixels",
        run.sweep.captures.len(),
        run.sweep.gains.len(),
        run.defects.len()
    );
    Ok(())
}

fn run_processed_command(a: Args) -> styx_calibrate::Result<()> {
    if a.dirs.is_empty() {
        return err(format!(
            "check-processed needs at least one directory\n{USAGE}"
        ));
    }
    let stats: Vec<processed::ProcessedStats> = a
        .dirs
        .iter()
        .map(|d| processed::check(d))
        .collect::<styx_calibrate::Result<_>>()?;
    let text = report::processed(&stats);
    match a.out {
        Some(p) => write(&p, &text)?,
        None => print!("{text}"),
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if matches!(cmd.as_str(), "help" | "-h" | "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let result = parse(rest).and_then(|a| match cmd.as_str() {
        "dark" => run_dark_command(a),
        "check-processed" => run_processed_command(a),
        _ => err(format!("unknown command {cmd}\n{USAGE}")),
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("styx-calibrate: {e}");
            ExitCode::FAILURE
        }
    }
}
