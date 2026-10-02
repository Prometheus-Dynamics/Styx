//! `styx-compare`: the same capture through Styx's libcamera backend and through the native
//! backend, measured the same way.
//!
//! ```sh
//! # one path per process, so CPU and memory belong to that path alone
//! styx-compare run --backend libcamera --format NV12 --label libcamera-nv12 --out lc.json
//! STYX_SENSOR_PATH=ov9782.toml styx-compare run --backend native --format pBAA --out native.json
//! # side by side
//! styx-compare report --json compare.json --md compare.md lc.json native.json
//! ```
//!
//! Per path: start latency (open → first frame; open → AE converged and → exposure settled,
//! over `--repeat` opens), frame rate accuracy and interval jitter, dropped frames (sequence
//! gaps and long intervals), CPU of the process and its children (libcamera's IPA proxy),
//! RSS/PSS and dma-bufs, and statistics of the last frame (saved with `--save`).
//!
//! Convergence: libcamera reports `AeState` per request; on the native path pass
//! `--ae-state-control 0xF4000010` (`styx`'s native `controls::AE_STATE`, published by the
//! processed NV12/RG24 modes). "Settled" needs only per-frame exposure and gain, which both
//! paths report.
//!
//! `tools/compare/device-run.sh` runs the set on the CM5.

mod capture;
mod image;
mod proc;
mod report;
mod timing;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use styx::prelude::*;
use styx_softisp::CfaPattern;

use capture::{RunConfig, RunResult};

const USAGE: &str = "usage:
  styx-compare run --backend <libcamera|native|...> [--format FOURCC] [--size 1280x800]
      [--fps 30] [--frames 300] [--warmup 15] [--repeat 3] [--converge-timeout-ms 4000]
      [--settle-window 6] [--ae-state-control ID|none] [--cfa rggb|bggr|grbg|gbrg]
      [--label NAME] [--out result.json] [--save frame.bin]
  styx-compare report [--json combined.json] [--md table.md] result.json...";

fn parse_u32(s: &str) -> Result<u32, String> {
    match s.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => s.parse(),
    }
    .map_err(|e| format!("{s}: {e}"))
}

fn parse_run(args: &[String]) -> Result<(RunConfig, Option<PathBuf>), String> {
    let mut cfg = RunConfig {
        label: String::new(),
        backend: BackendKind::Libcamera,
        format: None,
        width: 1280,
        height: 800,
        fps: 30,
        repeat: 3,
        warmup: 15,
        frames: 300,
        converge_timeout: Duration::from_millis(4000),
        ae_state_control: None,
        settle_window: 6,
        cfa: None,
        save: None,
    };
    let mut ae: Option<Option<ControlId>> = None;
    let mut out = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--backend" => cfg.backend = value()?.parse().map_err(|e| format!("{e:?}"))?,
            "--format" => {
                let v = value()?;
                let bytes: [u8; 4] = v
                    .as_bytes()
                    .try_into()
                    .map_err(|_| format!("format {v} is not four characters"))?;
                cfg.format = Some(FourCc::new(bytes));
            }
            "--size" => {
                let v = value()?;
                let (w, h) = v.split_once('x').ok_or(format!("bad size {v}"))?;
                cfg.width = parse_u32(w)?;
                cfg.height = parse_u32(h)?;
            }
            "--fps" => cfg.fps = parse_u32(&value()?)?,
            "--frames" => cfg.frames = parse_u32(&value()?)? as usize,
            "--warmup" => cfg.warmup = parse_u32(&value()?)? as usize,
            "--repeat" => cfg.repeat = parse_u32(&value()?)? as usize,
            "--settle-window" => cfg.settle_window = parse_u32(&value()?)?.max(2) as usize,
            "--converge-timeout-ms" => {
                cfg.converge_timeout = Duration::from_millis(u64::from(parse_u32(&value()?)?))
            }
            "--ae-state-control" => {
                let v = value()?;
                ae = Some(if v == "none" {
                    None
                } else {
                    Some(ControlId(parse_u32(&v)?))
                });
            }
            "--cfa" => {
                cfg.cfa = Some(match value()?.as_str() {
                    "rggb" => CfaPattern::Rggb,
                    "bggr" => CfaPattern::Bggr,
                    "grbg" => CfaPattern::Grbg,
                    "gbrg" => CfaPattern::Gbrg,
                    other => return Err(format!("unknown CFA pattern {other}")),
                })
            }
            "--label" => cfg.label = value()?,
            "--out" => out = Some(PathBuf::from(value()?)),
            "--save" => cfg.save = Some(PathBuf::from(value()?)),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    // libcamera reports AeState (control 2) in every request's metadata.
    cfg.ae_state_control = ae.unwrap_or(if cfg.backend == BackendKind::Libcamera {
        Some(capture::LC_AE_STATE)
    } else {
        None
    });
    if cfg.label.is_empty() {
        cfg.label = match cfg.format {
            Some(f) => format!("{}-{f}", cfg.backend),
            None => cfg.backend.to_string(),
        };
    }
    Ok((cfg, out))
}

fn write(path: &PathBuf, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn cmd_run(args: &[String]) -> Result<(), String> {
    let (cfg, out) = parse_run(args)?;
    let result = capture::run(&cfg).map_err(|e| format!("{}: {e}", cfg.label))?;
    let json = serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?;
    match out {
        Some(path) => write(&path, &json)?,
        None => println!("{json}"),
    }
    eprintln!("{}", report::markdown(std::slice::from_ref(&result)));
    Ok(())
}

fn cmd_report(args: &[String]) -> Result<(), String> {
    let (mut json_out, mut md_out, mut inputs) = (None, None, Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json_out = it.next().map(PathBuf::from),
            "--md" => md_out = it.next().map(PathBuf::from),
            path => inputs.push(PathBuf::from(path)),
        }
    }
    let runs: Vec<RunResult> = inputs
        .iter()
        .map(|p| {
            let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))
        })
        .collect::<Result<_, String>>()?;
    let md = report::markdown(&runs);
    if let Some(path) = json_out {
        let all = serde_json::json!({ "runs": runs });
        write(
            &path,
            &serde_json::to_string_pretty(&all).map_err(|e| e.to_string())?,
        )?;
    }
    match md_out {
        Some(path) => write(&path, &md)?,
        None => print!("{md}"),
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("report") => cmd_report(&args[1..]),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn run_arguments() {
        let (cfg, out) = parse_run(&args(
            "--backend libcamera --format NV12 --size 640x480 --out a.json",
        ))
        .unwrap();
        assert_eq!(cfg.backend, BackendKind::Libcamera);
        assert_eq!(cfg.format, Some(FourCc::new(*b"NV12")));
        assert_eq!((cfg.width, cfg.height), (640, 480));
        assert_eq!(cfg.ae_state_control, Some(ControlId(2)));
        assert_eq!(cfg.label, "libcamera-NV12");
        assert_eq!(out, Some(PathBuf::from("a.json")));
        let (cfg, _) = parse_run(&args("--backend virtual --ae-state-control 0xF4000010")).unwrap();
        assert_eq!(cfg.ae_state_control, Some(ControlId(0xF400_0010)));
        assert!(parse_run(&args("--format NV1")).is_err());
        assert!(parse_run(&args("--bogus")).is_err());
    }
}
