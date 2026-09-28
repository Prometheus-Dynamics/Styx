//! Command line.

use std::path::PathBuf;
use std::time::Duration;

/// Usage text.
pub const USAGE: &str = "\
native-spike: stream raw OV9782 frames through the Styx sensor bridge, sensor driven over I2C

usage: native-spike [options]

  --dry-run               read-only checks: description, media graph, formats, bridge; changes nothing
  --description PATH      sensor description (default: ov9782.toml)
  --mode NAME             sensor mode (default: 1280x800)
  --format NAME           description format (default: raw10)
  --frames N              frames for the baseline run (default: 120)
  --buffers N             MMAP buffers (default: 4)
  --fps LIST              frame rates to check, comma separated (default: 30,60,120)
  --exposure-fps F        frame rate during the exposure check (default: 30)
  --no-fps-check          skip the frame rate check
  --no-exposure-check     skip the exposure check
  --out-dir DIR           where the raw frame and PGM go (default: /tmp)
  --i2c-bus N             I2C adapter (default: from the bridge)
  --ack-timeout-ms MS     bridge acknowledgement timeout (default: 1000)
  --power-settle-ms MS    wait after switching the bridge's power on (default: 5)
  --row-step N            mean level from every N-th line (default: 4)
  -h, --help              this text
";

/// Parsed options.
#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    /// Only read-only checks.
    pub dry_run: bool,
    /// Sensor description.
    pub description: PathBuf,
    /// Mode name.
    pub mode: String,
    /// Format name.
    pub format: String,
    /// Baseline frames.
    pub frames: usize,
    /// Buffers.
    pub buffers: u32,
    /// Frame rates to check.
    pub fps: Vec<f64>,
    /// Frame rate for the exposure check.
    pub exposure_fps: f64,
    /// Run the frame rate check.
    pub fps_check: bool,
    /// Run the exposure check.
    pub exposure_check: bool,
    /// Output directory.
    pub out_dir: PathBuf,
    /// I²C bus override.
    pub i2c_bus: Option<u32>,
    /// Bridge acknowledgement timeout.
    pub ack_timeout: Duration,
    /// Settle time after power on.
    pub power_settle: Duration,
    /// Line step for mean levels.
    pub row_step: usize,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            dry_run: false,
            description: PathBuf::from("ov9782.toml"),
            mode: "1280x800".into(),
            format: "raw10".into(),
            frames: 120,
            buffers: 4,
            fps: vec![30.0, 60.0, 120.0],
            exposure_fps: 30.0,
            fps_check: true,
            exposure_check: true,
            out_dir: PathBuf::from("/tmp"),
            i2c_bus: None,
            ack_timeout: Duration::from_millis(1000),
            power_settle: Duration::from_millis(5),
            row_step: 4,
        }
    }
}

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Run with these options.
    Run(Args),
    /// Print usage.
    Help,
}

fn number<T: std::str::FromStr>(flag: &str, v: &str) -> Result<T, String> {
    v.parse()
        .map_err(|_| format!("{flag}: '{v}' is not a valid number"))
}

fn positive_f64(flag: &str, v: &str) -> Result<f64, String> {
    let f: f64 = number(flag, v)?;
    if f.is_finite() && f > 0.0 {
        Ok(f)
    } else {
        Err(format!("{flag}: '{v}' must be a positive number"))
    }
}

/// Parses arguments (without the program name).
pub fn parse<I, S>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut a = Args::default();
    let mut it = args.into_iter().map(Into::into);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--dry-run" => a.dry_run = true,
            "--no-fps-check" => a.fps_check = false,
            "--no-exposure-check" => a.exposure_check = false,
            "--description" => a.description = value()?.into(),
            "--mode" => a.mode = value()?,
            "--format" => a.format = value()?,
            "--out-dir" => a.out_dir = value()?.into(),
            "--frames" => a.frames = number(&flag, &value()?)?,
            "--buffers" => a.buffers = number(&flag, &value()?)?,
            "--i2c-bus" => a.i2c_bus = Some(number(&flag, &value()?)?),
            "--row-step" => a.row_step = number(&flag, &value()?)?,
            "--exposure-fps" => a.exposure_fps = positive_f64(&flag, &value()?)?,
            "--ack-timeout-ms" => {
                a.ack_timeout = Duration::from_millis(number(&flag, &value()?)?);
            }
            "--power-settle-ms" => {
                a.power_settle = Duration::from_millis(number(&flag, &value()?)?);
            }
            "--fps" => {
                a.fps = value()?
                    .split(',')
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| positive_f64(&flag, s.trim()))
                    .collect::<Result<_, _>>()?;
            }
            other => return Err(format!("unknown argument '{other}' (see --help)")),
        }
    }
    if !(2..=32).contains(&a.buffers) {
        return Err(format!("--buffers: {} (2..=32)", a.buffers));
    }
    if a.frames < 2 {
        return Err("--frames: at least 2".into());
    }
    if a.row_step == 0 {
        return Err("--row-step: at least 1".into());
    }
    if !(10..=10_000).contains(&a.ack_timeout.as_millis()) {
        return Err("--ack-timeout-ms: 10..=10000".into());
    }
    Ok(Command::Run(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Args, String> {
        match parse(args.iter().copied())? {
            Command::Run(a) => Ok(a),
            Command::Help => Err("help".into()),
        }
    }

    #[test]
    fn defaults() {
        let a = run(&[]).unwrap();
        assert_eq!(a, Args::default());
        assert!(!a.dry_run && a.fps_check && a.exposure_check);
        assert_eq!(a.fps, [30.0, 60.0, 120.0]);
    }

    #[test]
    fn options() {
        let a = run(&[
            "--dry-run",
            "--description",
            "/tmp/s/ov9782.toml",
            "--fps",
            "15, 90",
            "--frames",
            "30",
            "--i2c-bus",
            "10",
            "--ack-timeout-ms",
            "2500",
            "--no-exposure-check",
            "--out-dir",
            "/tmp/out",
        ])
        .unwrap();
        assert!(a.dry_run && !a.exposure_check && a.fps_check);
        assert_eq!(a.description, PathBuf::from("/tmp/s/ov9782.toml"));
        assert_eq!(a.fps, [15.0, 90.0]);
        assert_eq!((a.frames, a.i2c_bus), (30, Some(10)));
        assert_eq!(a.ack_timeout, Duration::from_millis(2500));
        assert_eq!(a.out_dir, PathBuf::from("/tmp/out"));
        assert_eq!(parse(["-h"]).unwrap(), Command::Help);
    }

    #[test]
    fn errors() {
        for bad in [
            &["--frames"][..],
            &["--frames", "x"],
            &["--fps", "30,-1"],
            &["--buffers", "1"],
            &["--ack-timeout-ms", "5"],
            &["--row-step", "0"],
            &["--exposure-fps", "nan"],
            &["--bogus"],
        ] {
            assert!(run(bad).is_err(), "{bad:?}");
        }
    }
}
