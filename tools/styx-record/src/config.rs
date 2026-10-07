//! Command line: what to record, from where, for how long.

use std::path::PathBuf;
use std::time::Duration;

pub const USAGE: &str = "usage:
  styx-record [record] --out DIR [options]

  where from:
    --source service|direct   a Styx camera service (default) or the camera itself
    --direct                  same as --source direct
    --socket PATH             camera service socket ($STYX_SOCKET, else /tmp/styx-camera.sock)
    --camera NAME             camera name, part of it, or an identity key (default: the first)
  what:
    --mode every|latest       every frame, reporting drops (default), or the newest frame only
    --size WxH                frame size (default: the camera's largest mode, its native size)
    --fps N                   frame rate (direct mode; a service client takes the running rate)
  how long (video):
    --seconds S               stop after S seconds
    --frames N                stop after N frames
                              (neither: until Ctrl-C)
  stills (one file per still, for calibration):
    --stills N                take N stills, then stop
    --on-key                  one still per Enter on stdin
    --every-secs K            one still every K seconds
  output:
    --out DIR                 directory for the files (created)
    --name NAME               file name stem (default: DIR's name)
    --queue N                 frames the disk writer may queue (default 32)
    --force                   overwrite existing files
    --quiet                   no progress lines

Writes <name>_<W>x<H>_gray.raw (Eidos raw grey video: frames back to back, no header),
<name>.frames.csv (per-frame sequence, timestamp, clock, drops) and <name>.json (camera
settings). See docs/recording.md.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Every frame, queued; drops are counted.
    Every,
    /// The newest frame only.
    Latest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Service,
    Direct,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StillTrigger {
    /// A line on stdin (Enter).
    Key,
    /// A fixed period.
    Every(Duration),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stills {
    pub count: u32,
    pub trigger: StillTrigger,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub source: SourceKind,
    pub socket: PathBuf,
    pub camera: Option<String>,
    pub mode: Mode,
    pub size: Option<(u32, u32)>,
    pub fps: Option<u32>,
    pub seconds: Option<f64>,
    pub frames: Option<u64>,
    pub stills: Option<Stills>,
    pub out: PathBuf,
    pub name: String,
    pub queue: usize,
    pub force: bool,
    pub quiet: bool,
    /// How often the camera's controls are read back while recording.
    pub settings_poll: Duration,
}

impl Config {
    /// A configuration recording from the service at `socket` into `out`, every frame.
    pub fn new(socket: impl Into<PathBuf>, out: impl Into<PathBuf>) -> Self {
        let out = out.into();
        let name = default_name(&out);
        Self {
            source: SourceKind::Service,
            socket: socket.into(),
            camera: None,
            mode: Mode::Every,
            size: None,
            fps: None,
            seconds: None,
            frames: None,
            stills: None,
            out,
            name,
            queue: 32,
            force: false,
            quiet: true,
            settings_poll: Duration::from_secs(1),
        }
    }

    /// Parse the command line (without the program name).
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut args = args.into_iter().peekable();
        if args.peek().is_some_and(|a| a == "record") {
            args.next();
        }
        let socket =
            std::env::var("STYX_SOCKET").unwrap_or_else(|_| "/tmp/styx-camera.sock".into());
        let mut c = Self::new(socket, PathBuf::new());
        c.quiet = false;
        let (mut out, mut name, mut stills, mut on_key, mut every) =
            (None, None, None, false, None);
        while let Some(arg) = args.next() {
            let mut value = |what: &str| args.next().ok_or_else(|| format!("{arg} needs {what}"));
            match arg.as_str() {
                "--source" => {
                    c.source = match value("service|direct")?.as_str() {
                        "service" => SourceKind::Service,
                        "direct" => SourceKind::Direct,
                        other => return Err(format!("--source {other}: service or direct")),
                    }
                }
                "--direct" => c.source = SourceKind::Direct,
                "--socket" => c.socket = value("a path")?.into(),
                "--camera" => c.camera = Some(value("a name")?),
                "--mode" => {
                    c.mode = match value("every|latest")?.as_str() {
                        "every" => Mode::Every,
                        "latest" => Mode::Latest,
                        other => return Err(format!("--mode {other}: every or latest")),
                    }
                }
                "--size" => {
                    let v = value("WxH")?;
                    let (w, h) = v.split_once('x').ok_or(format!("--size {v}: WxH"))?;
                    c.size = Some((num(&v, w)?, num(&v, h)?));
                }
                "--fps" => c.fps = Some(num("--fps", &value("a rate")?)?),
                "--seconds" => {
                    let s: f64 = num("--seconds", &value("seconds")?)?;
                    if !(s > 0.0 && s.is_finite()) {
                        return Err("--seconds must be positive".into());
                    }
                    c.seconds = Some(s);
                }
                "--frames" => c.frames = Some(num("--frames", &value("a count")?)?),
                "--stills" => stills = Some(num::<u32>("--stills", &value("a count")?)?),
                "--on-key" => on_key = true,
                "--every-secs" => {
                    let s: f64 = num("--every-secs", &value("seconds")?)?;
                    if !(s > 0.0 && s.is_finite()) {
                        return Err("--every-secs must be positive".into());
                    }
                    every = Some(Duration::from_secs_f64(s));
                }
                "--out" => out = Some(PathBuf::from(value("a directory")?)),
                "--name" => name = Some(value("a name")?),
                "--queue" => c.queue = num::<usize>("--queue", &value("a count")?)?.max(1),
                "--force" => c.force = true,
                "--quiet" => c.quiet = true,
                "-h" | "--help" => return Err(USAGE.into()),
                other => return Err(format!("unknown argument {other}\n\n{USAGE}")),
            }
        }
        c.out = out.ok_or(format!("--out DIR is required\n\n{USAGE}"))?;
        c.name = match name {
            Some(n) => n,
            None => default_name(&c.out),
        };
        if c.name.is_empty() || c.name.contains('/') {
            return Err(format!("bad name {:?} (use --name)", c.name));
        }
        c.stills = match (stills, on_key, every) {
            (None, false, None) => None,
            (Some(count), true, None) => Some(Stills {
                count,
                trigger: StillTrigger::Key,
            }),
            (Some(count), false, Some(period)) => Some(Stills {
                count,
                trigger: StillTrigger::Every(period),
            }),
            (Some(_), false, None) => return Err("--stills needs --on-key or --every-secs".into()),
            (_, true, Some(_)) => return Err("--on-key and --every-secs exclude each other".into()),
            (None, _, _) => return Err("--on-key and --every-secs need --stills N".into()),
        };
        if c.stills.is_some_and(|s| s.count == 0) {
            return Err("--stills must be at least 1".into());
        }
        if c.stills.is_some() && c.frames.is_some() {
            return Err("--frames is for video; stills stop after --stills N".into());
        }
        Ok(c)
    }
}

fn num<T: std::str::FromStr>(what: &str, v: &str) -> Result<T, String>
where
    T::Err: std::fmt::Display,
{
    v.parse().map_err(|e| format!("{what} {v}: {e}"))
}

/// The output directory's last component.
fn default_name(out: &std::path::Path) -> String {
    out.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "recording".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Config, String> {
        Config::parse(line.split_whitespace().map(String::from))
    }

    #[test]
    fn parses_a_service_recording() {
        let c = parse("record --camera front --seconds 30 --out /data/rec/x").unwrap();
        assert_eq!(c.camera.as_deref(), Some("front"));
        assert_eq!(c.seconds, Some(30.0));
        assert_eq!(c.out, PathBuf::from("/data/rec/x"));
        assert_eq!(c.name, "x");
        assert_eq!(c.mode, Mode::Every);
        assert_eq!(c.source, SourceKind::Service);
        assert!(c.stills.is_none());
    }

    #[test]
    fn parses_stills_and_direct_mode() {
        let c = parse("--direct --stills 5 --every-secs 2 --out d --mode latest").unwrap();
        assert_eq!(c.source, SourceKind::Direct);
        assert_eq!(c.mode, Mode::Latest);
        assert_eq!(
            c.stills,
            Some(Stills {
                count: 5,
                trigger: StillTrigger::Every(Duration::from_secs(2))
            })
        );
        let c = parse("--stills 3 --on-key --out d --size 1280x800").unwrap();
        assert_eq!(c.stills.unwrap().trigger, StillTrigger::Key);
        assert_eq!(c.size, Some((1280, 800)));
    }

    #[test]
    fn refuses_bad_combinations() {
        assert!(parse("--seconds 3").is_err());
        assert!(parse("--out d --stills 3").is_err());
        assert!(parse("--out d --on-key").is_err());
        assert!(parse("--out d --stills 3 --on-key --every-secs 1").is_err());
        assert!(parse("--out d --mode fast").is_err());
        assert!(parse("--out d --seconds -1").is_err());
        assert!(parse("--out d --bogus").is_err());
    }
}
