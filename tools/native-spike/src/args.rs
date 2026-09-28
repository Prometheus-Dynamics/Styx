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

verification (native path unless noted; raw register writes around the control scheduler):
  --write LIST            raw writes after the mode, before streaming: addr=value[:bytes],...
  --embedded              capture rp1-cfe-embedded too (bridge with the -emb overlay) and decode
  --regdump               read back what the description wrote and diagnostic registers
  --describe              row bands, statistics and 2x2 phase means of one frame
  --test-patterns         the description's test patterns and a solid pattern of known values
  --black                 black level at 1 line and 1x gain
  --delays                measure exposure, gain and frame length delays (raw writes)
  --group-hold            does a grouped exposure+gain change land on one frame (none/0x3308/0x3208)
  --sweep-exposure LIST   exposure sweep, lines (gain --base-gain), fitted
  --sweep-gain LIST       gain sweep, codes (exposure --base-exposure), fitted
  --base-exposure N       base exposure for the above, lines (default: 642)
  --base-gain C           base gain code for the above (default: 0x10)
  --verify-fps F          frame rate during the verification steps (default: 30)
  --kernel LIST           kernel driver path instead (ov9282 bound, no bridge): stream with each
                          exposure:gain setting (numbers or max), report levels, save frames
  --kernel-vblank N       vblank for --kernel (default: 1022)
  --analyse FILE          analyse a saved 1280x800 raw10 frame (stride 1600) and exit
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
    /// Verification steps.
    pub verify: Verify,
}

/// Verification options.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Verify {
    /// Raw writes after the mode.
    pub writes: Vec<crate::verify::RawWrite>,
    /// Capture embedded data.
    pub embedded: bool,
    /// Register read-back.
    pub regdump: bool,
    /// Describe one frame.
    pub describe: bool,
    /// Test patterns.
    pub test_patterns: bool,
    /// Black level.
    pub black: bool,
    /// Delay measurement.
    pub delays: bool,
    /// Group hold test.
    pub group_hold: bool,
    /// Exposure sweep values (lines).
    pub sweep_exposure: Vec<u32>,
    /// Gain sweep values (codes).
    pub sweep_gain: Vec<u32>,
    /// Base exposure (lines).
    pub base_exposure: u32,
    /// Base gain code.
    pub base_gain: u32,
    /// Frame rate during verification.
    pub fps: f64,
    /// Kernel path settings.
    pub kernel: Option<Vec<crate::kernel_path::KernelSetting>>,
    /// Kernel path vblank.
    pub kernel_vblank: i64,
    /// Frame file to analyse.
    pub analyse: Option<PathBuf>,
}

impl Verify {
    /// Whether any native verification step runs.
    pub fn any(&self) -> bool {
        self.regdump
            || self.embedded
            || self.describe
            || self.test_patterns
            || self.black
            || self.delays
            || self.group_hold
            || !self.sweep_exposure.is_empty()
            || !self.sweep_gain.is_empty()
    }
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
            verify: Verify {
                base_exposure: 642,
                base_gain: 0x10,
                fps: 30.0,
                kernel_vblank: 1022,
                ..Default::default()
            },
        }
    }
}

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Run with these options.
    Run(Box<Args>),
    /// Print usage.
    Help,
}

fn number<T: std::str::FromStr>(flag: &str, v: &str) -> Result<T, String> {
    v.parse()
        .map_err(|_| format!("{flag}: '{v}' is not a valid number"))
}

fn number_list(flag: &str, v: &str) -> Result<Vec<u32>, String> {
    v.split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| hex_u32(flag, s.trim()))
        .collect()
}

fn hex_u32(flag: &str, v: &str) -> Result<u32, String> {
    match v.strip_prefix("0x") {
        Some(h) => u32::from_str_radix(h, 16).map_err(|_| format!("{flag}: '{v}' is not a number")),
        None => number(flag, v),
    }
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
            "--write" => a.verify.writes = crate::verify::parse_writes(&value()?)?,
            "--regdump" => a.verify.regdump = true,
            "--embedded" => a.verify.embedded = true,
            "--describe" => a.verify.describe = true,
            "--test-patterns" => a.verify.test_patterns = true,
            "--black" => a.verify.black = true,
            "--delays" => a.verify.delays = true,
            "--group-hold" => a.verify.group_hold = true,
            "--sweep-exposure" => a.verify.sweep_exposure = number_list(&flag, &value()?)?,
            "--sweep-gain" => a.verify.sweep_gain = number_list(&flag, &value()?)?,
            "--base-exposure" => a.verify.base_exposure = hex_u32(&flag, &value()?)?,
            "--base-gain" => a.verify.base_gain = hex_u32(&flag, &value()?)?,
            "--verify-fps" => a.verify.fps = positive_f64(&flag, &value()?)?,
            "--kernel" => {
                a.verify.kernel = Some(crate::kernel_path::parse_settings(&value()?)?);
            }
            "--kernel-vblank" => a.verify.kernel_vblank = number(&flag, &value()?)?,
            "--analyse" => a.verify.analyse = Some(value()?.into()),
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
    Ok(Command::Run(Box::new(a)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Args, String> {
        match parse(args.iter().copied())? {
            Command::Run(a) => Ok(*a),
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

    #[test]
    fn verification_options() {
        let a = run(&[
            "--sweep-exposure",
            "100,0x200",
            "--base-gain",
            "0x20",
            "--delays",
            "--write",
            "0x4307=0x31",
            "--kernel",
            "max:max",
        ])
        .unwrap();
        let v = &a.verify;
        assert_eq!(v.sweep_exposure, [100, 512]);
        assert_eq!((v.base_gain, v.base_exposure), (0x20, 642));
        assert!(v.delays && v.any() && v.kernel.is_some());
        assert_eq!(v.writes, [(0x4307, 0x31, 1)]);
        assert!(!Args::default().verify.any());
        assert!(run(&["--sweep-gain", "1,x"]).is_err());
    }
}
