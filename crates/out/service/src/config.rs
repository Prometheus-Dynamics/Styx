//! The config file: what to serve, how, where its sockets live, and the failure policy.
//!
//! The file is TOML. Every key is optional except `socket_dir`. Unknown keys are errors.
//!
//! ```toml
//! # Where the control socket lives (the default is `<socket_dir>/control.sock`).
//! socket_dir = "/run/styx"
//! # control_socket = "/run/styx/control.sock"
//!
//! # Who may connect. Every socket (control and per-camera) gets this group and mode, whatever the
//! # umask. Default: group = the service's own group, mode = "660" (owner and group only). The
//! # directories holding the sockets get the traversal bits that mode needs (750 by default). A
//! # group name is looked up in /etc/group; a number is used as given. The service's user must be
//! # in the group, or starting fails naming the key.
//! # socket_group = "styx"
//! # socket_mode = "660"
//!
//! # The Lemnos daemon that holds the native cameras. Without it no native camera is listed.
//! lemnos_socket = "/run/lemnos/lemnosd.sock"
//!
//! # Cameras to serve, by their id (for example "lemnos:sensor0", "usb:0000:00:14.0-2",
//! # "libcamera:...", "/dev/video2"). Empty means every camera found.
//! cameras = []
//!
//! # Force every camera to one backend. A camera the backend does not list makes the scan fail
//! # (the error is in the status). A `[camera.<id>]` `force` wins over this.
//! # force = "native"
//!
//! # Virtual V4L2 nodes (loopback, fed by another program) are used only when named here.
//! # virtual_nodes = ["/dev/video9"]
//!
//! # Directories of tuning files for the 3A loop (`aaa`), searched in order first; then
//! # STYX_TUNING_PATH, /etc/styx/tuning and the bundled /usr/share/styx/tuning.
//! tuning_dirs = ["/opt/board/tuning"]
//!
//! # How often the cameras are listed again, in milliseconds.
//! scan_ms = 2000
//!
//! # The retry backoff: the first wait, then doubling, capped. The cap must be 5 to 10 seconds.
//! backoff_first_ms = 250
//! backoff_cap_ms = 5000
//!
//! # Configured policy only: turn a camera off after this many failures in a row. Absent means
//! # never: a camera that drops is served again. A turned-off camera waits for a manual `start`.
//! # auto_off_after = 50
//!
//! # Per camera. The key is the camera's id. Sizes and rates are passed to the backend as given.
//! [camera."lemnos:sensor0"]
//! width = 1280
//! height = 800
//! format = "pRAA"        # native: optional; the frame format the mode must have (pRAA is 10-bit
//!                        # RGGB packed, RGGB is 8-bit). Asking for a format no mode has is an error.
//! bit_depth = 10         # native: optional; the bit depth the mode must have
//! fps_milli = 60000      # thousandths of a frame per second (native: required)
//! exposure_us = 10000    # native: the sensor's initial exposure (clamped to what the frame allows)
//! gain_milli = 1000      # native: the sensor's initial analogue gain (1000 is 1.0x)
//! # force = "native"
//!
//! [camera."/dev/video2"]
//! width = 640
//! height = 480
//! format = "YUYV"        # V4L2 and libcamera: the pixel format's four-character code
//! fps_milli = 30000
//! buffers = 4            # V4L2 and libcamera: buffers requested
//! hardware_reserve = 4   # optional: pool buffers the camera keeps to stream (default: half, min 2);
//!                        # all readers together may hold buffers - hardware_reserve - 1 frames older than the newest
//!
//! # A processed stream beside the raw one (see `isp`). Absent or "none": raw only.
//! [camera."lemnos:sensor1"]
//! isp = "auto"           # auto | pisp | soft | none
//! isp_format = "NV12"    # output 0's format (default NV12)
//! isp_size = "1280x800"  # output 0's size (default: the raw frame's)
//! isp_second = "NV12:640x400"   # output 1, published too (default: none published)
//! isp_refusals = 3       # auto: back-end refusals in a row that switch to the soft ISP
//! isp_retry_start_ms = 250      # auto: the first wait before the back end is tried again
//! isp_retry_cap_ms = 5000       # auto: the longest wait between tries (at most 10000)
//! aaa = true             # the 3A loop on the processed stream (default false; needs isp)
//! tuning = "<sensor>.toml"      # the tuning file it asks for (default: the generic one)
//! aaa_stats_every = 4    # the loop's statistics on one frame in this many (1..=30, default 4)
//! aaa_stats_rows = 4     # ... counted on one quad row in this many (1..=8, default 4)
//! ```
//!
//! Native cameras: `width` and `height` are the smallest mode that is acceptable, and `format` and
//! `bit_depth` pick the mode when they are set (only modes with that format or depth are
//! candidates). With neither set, the highest bit depth that reaches `fps_milli` is taken, and a
//! lower depth only when the rate needs it. A rate above the chosen mode's ceiling (its shortest
//! frame) is answered with the ceiling. The status line says which mode, and why (`mode_reason`).
//! An exposure the chosen mode's frame cannot hold (its shortest frame less the sensor's margin) is
//! moved to that ceiling by the sensor, which reports it, and the status says so (`exposure_clamp`);
//! the frame rate keeps its priority, so a frame is never lengthened for an exposure. A control request past the limit is
//! answered `ok` with the value applied (see `frame` and `list`), not refused.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use styx_native_run::{Backoff, BackoffError};
use styx_select::Backend;

/// The backend a camera may be forced to, as the config names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Force {
    Native,
    Libcamera,
    V4l2,
}

impl Force {
    /// The backend this names.
    pub const fn backend(self) -> Backend {
        match self {
            Force::Native => Backend::Native,
            Force::Libcamera => Backend::Libcamera,
            Force::V4l2 => Backend::V4l2,
        }
    }
}

/// One camera's requested settings. Every field is optional.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraConfig {
    /// Forces this camera's backend. Wins over the global `force`.
    #[serde(default)]
    pub force: Option<Force>,
    /// Requested width in pixels. Native: the mode must be at least this wide (0 accepts any).
    #[serde(default)]
    pub width: Option<u32>,
    /// Requested height in pixels. Native: the mode must be at least this tall (0 accepts any).
    #[serde(default)]
    pub height: Option<u32>,
    /// Requested pixel format's four-character code. V4L2 and libcamera: the format. Native: the
    /// format the sensor mode must have (`pRAA` for 10-bit RGGB, `RGGB` for 8-bit).
    #[serde(default)]
    pub format: Option<String>,
    /// Native: the bit depth the sensor mode must have. Absent: the choice rule decides.
    #[serde(default)]
    pub bit_depth: Option<u8>,
    /// Requested frame rate in thousandths of a frame per second, passed as given.
    #[serde(default)]
    pub fps_milli: Option<u32>,
    /// Native: the sensor's initial exposure in microseconds.
    #[serde(default)]
    pub exposure_us: Option<u32>,
    /// Native: the sensor's initial analogue gain in thousandths (1000 is 1.0x).
    #[serde(default)]
    pub gain_milli: Option<u32>,
    /// V4L2 and libcamera: buffers requested from the driver.
    #[serde(default)]
    pub buffers: Option<u32>,
    /// Buffers of the camera's pool the camera keeps for itself to stream (default: half the pool's
    /// buffers, at least 2, at most the pool's size). Frames the ring holds, in its window and held
    /// by readers, never take more than `buffers - hardware_reserve` of the pool, so the camera
    /// always has this many free. In plain words: all readers TOGETHER may hold up to `buffers -
    /// hardware_reserve - 1` frames older than the newest (a frame another reader has in hand counts
    /// too); past that the reader holding the oldest frame is cut off with a consumer fault. A lone
    /// reader may hold all of them. The ring's window of recent frames for late readers shrinks to
    /// make room and grows back when the holds are released.
    #[serde(default)]
    pub hardware_reserve: Option<u32>,
    /// Which ISP makes a processed stream beside the raw one: `auto` (the PiSP back end when the
    /// camera's chain has one that takes the geometry, else the soft ISP; refusals switch to the
    /// soft ISP and the back end is tried again), `pisp` (the back end or no processed stream),
    /// `soft`, or `none` (the default: raw only).
    #[serde(default)]
    pub isp: Option<IspMode>,
    /// Output 0's format (default `NV12`).
    #[serde(default)]
    pub isp_format: Option<String>,
    /// Output 0's size, `<width>x<height>` (default: the raw frame's).
    #[serde(default)]
    pub isp_size: Option<String>,
    /// Output 1, `<format>:<width>x<height>`, published on its own socket. Default: none.
    #[serde(default)]
    pub isp_second: Option<String>,
    /// `auto`: back-end refusals in a row that switch the camera to the soft ISP (default 3).
    #[serde(default)]
    pub isp_refusals: Option<u32>,
    /// `auto`: the first wait before the back end is tried again, in ms (default 250).
    #[serde(default)]
    pub isp_retry_start_ms: Option<u64>,
    /// `auto`: the longest wait between tries, in ms (default 5000, at most 10000).
    #[serde(default)]
    pub isp_retry_cap_ms: Option<u64>,
    /// Runs the 3A loop on the processed stream (needs `isp`; a native camera): auto exposure,
    /// auto white balance and flicker avoidance, each on, off or manual at run time (default
    /// false: exposure and gain stay as set, and the ISP runs neutral settings).
    #[serde(default)]
    pub aaa: Option<bool>,
    /// The tuning file the 3A loop asks for, by file name (a `.json` name looks for the `.toml` of
    /// the same stem first). Searched in `tuning_dirs`, then the system's directories (see
    /// `tuning_dirs`); absent or not found, the generic tuning, then styx-aaa's defaults.
    #[serde(default)]
    pub tuning: Option<String>,
    /// The 3A loop's statistics are taken on one frame in this many, and the loop steps on
    /// those frames (1..=30, default [`StatsSampling::DEFAULT_EVERY`]). The tunings' speeds are
    /// per step, as made for about 30 steps a second.
    #[serde(default)]
    pub aaa_stats_every: Option<u32>,
    /// The statistics are counted on one quad row in this many (1..=8, default
    /// [`StatsSampling::DEFAULT_ROWS`]; fewer where the frame has too few rows for every zone).
    #[serde(default)]
    pub aaa_stats_rows: Option<u32>,
}

/// How often and how densely the 3A loop's statistics are counted (`aaa_stats_every`,
/// `aaa_stats_rows`): their cost against how soon the loop follows the light.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsSampling {
    /// One frame in this many.
    pub every: u32,
    /// One quad row in this many.
    pub rows: u32,
}

impl StatsSampling {
    /// At 120 frames a second, 30 statistics a second: the rate the tunings' speeds are made for.
    pub const DEFAULT_EVERY: u32 = 4;
    /// Every 4th quad row (every other quad of it on the back end's route): about 31 quads in
    /// each of 32x32 zones at 1280x800, above the tunings' 16 a zone needs.
    pub const DEFAULT_ROWS: u32 = 4;
}

impl Default for StatsSampling {
    fn default() -> Self {
        Self {
            every: Self::DEFAULT_EVERY,
            rows: Self::DEFAULT_ROWS,
        }
    }
}

/// Which ISP makes a camera's processed stream (`isp`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IspMode {
    /// Raw only.
    #[default]
    None,
    /// The PiSP back end when the chain has one, else the soft ISP; see [`CameraConfig::isp`].
    Auto,
    /// The PiSP back end or nothing.
    Pisp,
    /// The soft ISP.
    Soft,
}

impl IspMode {
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Auto => "auto",
            Self::Pisp => "pisp",
            Self::Soft => "soft",
        }
    }
}

/// One ISP output as the config names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IspOutputSpec {
    pub code: styx_format::FourCc,
    /// `None`: the raw frame's size.
    pub size: Option<(u32, u32)>,
}

/// A camera's processed stream, checked: the outputs and the switch policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IspPlan {
    pub mode: IspMode,
    pub output0: IspOutputSpec,
    pub output1: Option<IspOutputSpec>,
    pub policy: crate::isp_switch::SwitchPolicy,
    /// The 3A loop's statistics.
    pub stats: StatsSampling,
}

fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (w, h) = text.split_once('x')?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

impl CameraConfig {
    /// The processed stream this camera asks for, `None` for raw only, or why the keys cannot be
    /// used (by key name).
    pub fn isp_plan(&self) -> Result<Option<IspPlan>, String> {
        let mode = self.isp.unwrap_or_default();
        if mode == IspMode::None {
            if self.aaa == Some(true) {
                return Err("aaa: the 3A loop runs on a processed stream; set isp".to_string());
            }
            return Ok(None);
        }
        let code = |text: &str, key: &str| {
            text.parse::<styx_format::FourCc>()
                .map_err(|_| format!("{key}: {text:?} is not a four-character code"))
        };
        let output0 = IspOutputSpec {
            code: code(self.isp_format.as_deref().unwrap_or("NV12"), "isp_format")?,
            size: match &self.isp_size {
                Some(text) => Some(
                    parse_size(text)
                        .ok_or_else(|| format!("isp_size: {text:?} is not <width>x<height>"))?,
                ),
                None => None,
            },
        };
        let output1 = match &self.isp_second {
            Some(text) => {
                let (fmt, size) = text.split_once(':').ok_or_else(|| {
                    format!("isp_second: {text:?} is not <format>:<width>x<height>")
                })?;
                Some(IspOutputSpec {
                    code: code(fmt, "isp_second")?,
                    size: Some(parse_size(size).ok_or_else(|| {
                        format!("isp_second: {text:?} is not <format>:<width>x<height>")
                    })?),
                })
            }
            None => None,
        };
        let defaults = crate::isp_switch::SwitchPolicy::default();
        let ms = std::time::Duration::from_millis;
        let policy = crate::isp_switch::SwitchPolicy::new(
            self.isp_refusals.unwrap_or(defaults.refusals),
            self.isp_retry_start_ms.map_or(defaults.retry_start, ms),
            self.isp_retry_cap_ms.map_or(defaults.retry_cap, ms),
        )?;
        let within = |v: Option<u32>, default: u32, max: u32, key: &str| match v {
            None => Ok(default),
            Some(v) if (1..=max).contains(&v) => Ok(v),
            Some(v) => Err(format!("{key}: {v} is not within 1..={max}")),
        };
        let stats = StatsSampling {
            every: within(
                self.aaa_stats_every,
                StatsSampling::DEFAULT_EVERY,
                30,
                "aaa_stats_every",
            )?,
            rows: within(
                self.aaa_stats_rows,
                StatsSampling::DEFAULT_ROWS,
                8,
                "aaa_stats_rows",
            )?,
        };
        Ok(Some(IspPlan {
            mode,
            output0,
            output1,
            policy,
            stats,
        }))
    }
}

/// A socket group as the config names it: a number (`socket_group = 1001`), or a name looked up
/// in `/etc/group` (`socket_group = "styx"`). A string of digits is a number too.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum GroupSpec {
    Gid(u32),
    Name(String),
}

/// The whole config file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where the sockets live.
    pub socket_dir: PathBuf,
    /// The control socket. The default is `<socket_dir>/control.sock`.
    #[serde(default)]
    pub control_socket: Option<PathBuf>,
    /// The group of every socket: a group name, or a numeric gid. Default: the service's own.
    #[serde(default)]
    pub socket_group: Option<GroupSpec>,
    /// The mode of every socket, in octal (for example "660"). Default: "660".
    #[serde(default)]
    pub socket_mode: Option<String>,
    /// The Lemnos daemon for native cameras.
    #[serde(default)]
    pub lemnos_socket: Option<PathBuf>,
    /// Forces every camera to one backend.
    #[serde(default)]
    pub force: Option<Force>,
    /// Virtual V4L2 nodes to use, by any of their names.
    #[serde(default)]
    pub virtual_nodes: Vec<String>,
    /// The cameras to serve, by id. Empty serves every camera found.
    #[serde(default)]
    pub cameras: Vec<String>,
    /// Directories of tuning files for the 3A loop, searched in order before `STYX_TUNING_PATH`,
    /// `/etc/styx/tuning` and the bundled `/usr/share/styx/tuning`.
    #[serde(default)]
    pub tuning_dirs: Vec<PathBuf>,
    /// How often the cameras are listed again, in milliseconds.
    #[serde(default = "default_scan_ms")]
    pub scan_ms: u64,
    /// The first retry wait, in milliseconds.
    #[serde(default = "default_backoff_first_ms")]
    pub backoff_first_ms: u64,
    /// The retry wait's cap, in milliseconds. Must be 5000 through 10000.
    #[serde(default = "default_backoff_cap_ms")]
    pub backoff_cap_ms: u64,
    /// Failures in a row after which a camera turns itself off. Absent: never.
    #[serde(default)]
    pub auto_off_after: Option<NonZeroU32>,
    /// Per-camera settings, by camera id.
    #[serde(default)]
    pub camera: BTreeMap<String, CameraConfig>,
}

fn default_scan_ms() -> u64 {
    2_000
}

fn default_backoff_first_ms() -> u64 {
    250
}

fn default_backoff_cap_ms() -> u64 {
    5_000
}

/// Why a config file could not be used.
#[derive(Debug)]
pub enum ConfigError {
    /// The file could not be read.
    Read(std::io::Error),
    /// The file is not valid TOML for this config, or names an unknown key.
    Parse(String),
    /// The backoff is not 5 to 10 seconds at the cap, or the first wait is zero.
    Backoff(BackoffError),
    /// A camera's format is not a four-character code.
    Format { camera: String, format: String },
    /// A socket setting (`socket_group` or `socket_mode`) cannot be used.
    Socket { key: &'static str, message: String },
    /// A camera's `isp` keys cannot be used.
    Isp { camera: String, message: String },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Read(e) => write!(f, "reading the config: {e}"),
            ConfigError::Parse(e) => write!(f, "config: {e}"),
            ConfigError::Backoff(e) => write!(f, "config backoff: {e}"),
            ConfigError::Format { camera, format } => write!(
                f,
                "camera {camera}: format {format:?} is not a four-character code"
            ),
            ConfigError::Socket { key, message } => write!(f, "config {key}: {message}"),
            ConfigError::Isp { camera, message } => write!(f, "camera {camera}: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Reads and checks the config at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(ConfigError::Read)?;
        Self::parse(&text)
    }

    /// Parses and checks config text.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.check()?;
        Ok(config)
    }

    /// Checks what parsing cannot: the backoff, the format codes, and the socket mode's syntax.
    /// The group is looked up when the service starts (see `access`).
    pub fn check(&self) -> Result<(), ConfigError> {
        self.backoff().map_err(ConfigError::Backoff)?;
        if let Some(mode) = &self.socket_mode {
            crate::access::parse_mode(mode).map_err(|message| ConfigError::Socket {
                key: "socket_mode",
                message,
            })?;
        }
        for (camera, settings) in &self.camera {
            if let Some(format) = &settings.format
                && format.parse::<styx_format::FourCc>().is_err()
            {
                return Err(ConfigError::Format {
                    camera: camera.clone(),
                    format: format.clone(),
                });
            }
            settings.isp_plan().map_err(|message| ConfigError::Isp {
                camera: camera.clone(),
                message,
            })?;
        }
        Ok(())
    }

    /// The retry backoff the config names.
    pub fn backoff(&self) -> Result<Backoff, BackoffError> {
        Backoff::new(
            std::time::Duration::from_millis(self.backoff_first_ms),
            std::time::Duration::from_millis(self.backoff_cap_ms),
        )
    }

    /// The control socket's path.
    pub fn control_socket(&self) -> PathBuf {
        self.control_socket
            .clone()
            .unwrap_or_else(|| self.socket_dir.join("control.sock"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_config_takes_the_defaults() {
        let config = Config::parse("socket_dir = \"/tmp/x\"\n").expect("parses");
        assert_eq!(config.scan_ms, 2_000);
        assert_eq!(config.backoff_cap_ms, 5_000);
        assert_eq!(config.auto_off_after, None);
        assert_eq!(
            config.control_socket(),
            PathBuf::from("/tmp/x/control.sock")
        );
    }

    #[test]
    fn unknown_keys_and_bad_backoffs_are_refused() {
        assert!(Config::parse("socket_dir = \"/x\"\nbogus = 1\n").is_err());
        assert!(matches!(
            Config::parse("socket_dir = \"/x\"\nbackoff_cap_ms = 4000\n"),
            Err(ConfigError::Backoff(_))
        ));
        assert!(matches!(
            Config::parse("socket_dir = \"/x\"\n[camera.\"a\"]\nformat = \"TOOLONG\"\n"),
            Err(ConfigError::Format { .. })
        ));
    }

    #[test]
    fn per_camera_settings_parse() {
        let config = Config::parse(
            "socket_dir = \"/x\"\nforce = \"v4l2\"\n[camera.\"lemnos:cam0\"]\nforce = \"native\"\nwidth = 320\nfps_milli = 60000\n",
        )
        .expect("parses");
        let cam = &config.camera["lemnos:cam0"];
        assert_eq!(cam.force, Some(Force::Native));
        assert_eq!(cam.width, Some(320));
        assert_eq!(cam.fps_milli, Some(60_000));
        assert_eq!(config.force, Some(Force::V4l2));
    }

    #[test]
    fn isp_keys_parse_to_a_plan_and_bad_ones_are_refused_by_name() {
        let config = Config::parse(
            "socket_dir = \"/x\"\n[camera.\"a\"]\nisp = \"auto\"\nisp_size = \"320x40\"\nisp_second = \"RGB3:160x20\"\nisp_refusals = 2\nisp_retry_start_ms = 50\nisp_retry_cap_ms = 400\n",
        )
        .expect("parses");
        let plan = config.camera["a"]
            .isp_plan()
            .expect("valid")
            .expect("a plan");
        assert_eq!(plan.mode, IspMode::Auto);
        assert_eq!(plan.output0.code, styx_format::FourCc::NV12);
        assert_eq!(plan.output0.size, Some((320, 40)));
        assert_eq!(
            plan.output1,
            Some(IspOutputSpec {
                code: styx_format::FourCc::RGB3,
                size: Some((160, 20))
            })
        );
        assert_eq!(plan.policy.refusals, 2);
        assert_eq!(
            plan.stats,
            StatsSampling { every: 4, rows: 4 },
            "the default sampling"
        );
        let sampled = Config::parse(
            "socket_dir = \"/x\"\n[camera.\"a\"]\nisp = \"soft\"\naaa_stats_every = 1\naaa_stats_rows = 8\n",
        )
        .expect("parses");
        assert_eq!(
            sampled.camera["a"]
                .isp_plan()
                .expect("valid")
                .expect("a plan")
                .stats,
            StatsSampling { every: 1, rows: 8 }
        );
        assert_eq!(
            Config::parse("socket_dir = \"/x\"\n[camera.\"a\"]\n")
                .expect("parses")
                .camera["a"]
                .isp_plan(),
            Ok(None),
            "absent is none: raw only"
        );
        for (keys, named) in [
            ("isp = \"auto\"\nisp_size = \"320\"\n", "isp_size"),
            ("isp = \"soft\"\nisp_second = \"NV12\"\n", "isp_second"),
            (
                "isp = \"auto\"\nisp_retry_cap_ms = 20000\n",
                "isp_retry_cap_ms",
            ),
            ("isp = \"auto\"\nisp_refusals = 0\n", "isp_refusals"),
            ("isp = \"soft\"\naaa_stats_every = 0\n", "aaa_stats_every"),
            ("isp = \"soft\"\naaa_stats_every = 31\n", "aaa_stats_every"),
            ("isp = \"soft\"\naaa_stats_rows = 9\n", "aaa_stats_rows"),
        ] {
            let err = Config::parse(&format!("socket_dir = \"/x\"\n[camera.\"a\"]\n{keys}"))
                .expect_err(keys);
            assert!(err.to_string().contains(named), "{err}");
        }
        assert!(Config::parse("socket_dir = \"/x\"\n[camera.\"a\"]\nisp = \"gpu\"\n").is_err());
    }
}
