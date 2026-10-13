//! `styx-iq-compare`: image-quality numbers for raw grey captures, side by side.
//!
//! ```text
//! styx-iq-compare [--width 1280] [--height 800] [--skip N] [--frames N]
//!                 [--noise-roi x0,y0,x1,y1] [--edge-roi x0,y0,x1,y1] FILE...
//! ```
//!
//! Defaults are the coordinator's regions: the static wall patch 560,120,760,300 for noise and
//! the AprilTag 5 region 622,180,658,222 for edges. See the library documentation for what each
//! number means.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::process::ExitCode;

use styx_iq_compare::{Report, Roi, analyse, frame_count};

const USAGE: &str = "usage: styx-iq-compare [--width N] [--height N] [--skip N] [--frames N] \
[--noise-roi x0,y0,x1,y1] [--edge-roi x0,y0,x1,y1] FILE...";

struct Options {
    width: usize,
    height: usize,
    skip: usize,
    frames: Option<usize>,
    noise: Roi,
    edge: Roi,
    files: Vec<String>,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        width: 1280,
        height: 800,
        skip: 0,
        frames: None,
        noise: Roi::parse("560,120,760,300")?,
        edge: Roi::parse("622,180,658,222")?,
        files: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        let number = |s: String| {
            s.parse::<usize>()
                .map_err(|_| format!("{arg}: `{s}` is not a whole number"))
        };
        match arg.as_str() {
            "--width" => o.width = number(value()?)?,
            "--height" => o.height = number(value()?)?,
            "--skip" => o.skip = number(value()?)?,
            "--frames" => o.frames = Some(number(value()?)?),
            "--noise-roi" => o.noise = Roi::parse(&value()?)?,
            "--edge-roi" => o.edge = Roi::parse(&value()?)?,
            "-h" | "--help" => return Err(USAGE.to_string()),
            s if s.starts_with("--") => return Err(format!("unknown option {s}\n{USAGE}")),
            _ => o.files.push(arg.clone()),
        }
    }
    if o.files.is_empty() {
        return Err(format!("no capture files given\n{USAGE}"));
    }
    Ok(o)
}

/// Runs the analysis on one file: skips `skip` frames and reads up to `frames` (all by default).
fn run(path: &str, o: &Options) -> Result<Report, String> {
    let mut file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let len = file.metadata().map_err(|e| format!("{path}: {e}"))?.len();
    let total = frame_count(len, o.width, o.height).map_err(|e| format!("{path}: {e}"))?;
    if o.skip > total {
        return Err(format!("{path}: --skip {} beyond {total} frames", o.skip));
    }
    let available = total - o.skip;
    let count = o.frames.map_or(available, |n| n.min(available));
    let frame_len = o.width * o.height;
    file.seek(SeekFrom::Start((o.skip * frame_len) as u64))
        .map_err(|e| format!("{path}: {e}"))?;
    let mut reader = BufReader::new(file);
    let frames = (0..count).map(|_| {
        let mut buf = vec![0u8; frame_len];
        reader
            .read_exact(&mut buf)
            .map(|()| buf)
            .map_err(|e| format!("{path}: {e}"))
    });
    analyse(frames, o.width, o.height, o.noise, o.edge).map_err(|e| format!("{path}: {e}"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let o = match parse_args(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let mut reports = Vec::new();
    for path in &o.files {
        match run(path, &o) {
            Ok(r) => reports.push((path.clone(), r)),
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        }
    }
    println!(
        "{}x{}, noise patch {}, edge region {}",
        o.width, o.height, o.noise, o.edge
    );
    let mut header = format!("{:<28}", "measure");
    for (path, _) in &reports {
        let name = std::path::Path::new(path)
            .file_name()
            .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned());
        header.push_str(&format!(" {:>22}", name));
    }
    println!("{header}");
    type Row = (&'static str, fn(&Report) -> f64);
    let rows: [Row; 11] = [
        ("frames", |r| r.frames as f64),
        ("mean grey (noise patch)", |r| r.mean),
        ("temporal std, mean (DN)", |r| r.temporal_std),
        ("temporal std, median (DN)", |r| r.temporal_std_median),
        ("temporal std, p90 (DN)", |r| r.temporal_std_p90),
        ("frame mean std (DN)", |r| r.frame_mean_std),
        ("spatial sigma (DN)", |r| r.spatial_sigma),
        ("edge p10", |r| r.edge_p10),
        ("edge p90", |r| r.edge_p90),
        ("edge contrast p90-p10", |r| r.edge_contrast),
        ("gradient energy", |r| r.gradient_energy),
    ];
    for (label, f) in rows {
        let mut line = format!("{label:<28}");
        for (_, r) in &reports {
            line.push_str(&format!(" {:>22.3}", f(r)));
        }
        println!("{line}");
    }
    ExitCode::SUCCESS
}
