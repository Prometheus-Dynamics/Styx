//! The config file: what to serve, how, where its sockets live, and the failure policy.
//!
//! The file is TOML. Every key is optional except `socket_dir`. Unknown keys are errors.
//!
//! ```toml
//! # Where the control socket lives (the default is `<socket_dir>/control.sock`).
//! socket_dir = "/run/styx"
//! # control_socket = "/run/styx/control.sock"
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
//! # Directories of tuning files. Accepted here; the 3A controller is not wired in this service.
//! tuning_dirs = ["/usr/share/styx/tuning", "/etc/styx/tuning"]
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
//! width = 1332
//! height = 990
//! fps_milli = 30000      # thousandths of a frame per second, as the backend's request
//!                        # (native: required, it is the mode's floor and the initial rate)
//! exposure_us = 10000    # native: the sensor's initial exposure
//! gain_milli = 1000      # native: the sensor's initial analogue gain (1000 is 1.0x)
//! # force = "native"
//!
//! [camera."/dev/video2"]
//! width = 640
//! height = 480
//! format = "YUYV"        # V4L2 and libcamera: the pixel format's four-character code
//! fps_milli = 30000
//! buffers = 4            # V4L2 and libcamera: buffers requested
//! ```
//!
//! Native cameras take their format from the sensor's mode; `format` is for V4L2 and libcamera
//! only, and a native camera that sets it gets an error at open.

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
    /// Requested pixel format's four-character code. V4L2 and libcamera only.
    #[serde(default)]
    pub format: Option<String>,
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
    /// Directories of tuning files (accepted; the 3A controller is not wired in this service).
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

    /// Checks what parsing cannot: the backoff and the format codes.
    pub fn check(&self) -> Result<(), ConfigError> {
        self.backoff().map_err(ConfigError::Backoff)?;
        for (camera, settings) in &self.camera {
            if let Some(format) = &settings.format
                && format.parse::<styx_format::FourCc>().is_err()
            {
                return Err(ConfigError::Format {
                    camera: camera.clone(),
                    format: format.clone(),
                });
            }
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
}
