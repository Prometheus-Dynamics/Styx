//! `BackendKind` names: display and parsing.

use crate::BackendKind;

impl std::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BackendKind::V4l2 => "v4l2",
            BackendKind::Libcamera => "libcamera",
            BackendKind::Virtual => "virtual",
            BackendKind::Netcam => "netcam",
            BackendKind::File => "file",
            BackendKind::Simulation => "simulation",
            BackendKind::Replay => "replay",
            BackendKind::Native => "native",
            BackendKind::Uvc => "uvc",
        })
    }
}

impl std::str::FromStr for BackendKind {
    type Err = BackendKindParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "v4l2" | "video4linux" | "video4linux2" => Ok(BackendKind::V4l2),
            "libcamera" => Ok(BackendKind::Libcamera),
            "virtual" => Ok(BackendKind::Virtual),
            "netcam" | "network" | "network-camera" => Ok(BackendKind::Netcam),
            "file" | "file-backend" => Ok(BackendKind::File),
            "simulation" | "simulation-bevy" => Ok(BackendKind::Simulation),
            "replay" => Ok(BackendKind::Replay),
            "native" | "styx-native" => Ok(BackendKind::Native),
            "uvc" | "usbfs" | "styx-uvc" | "userspace-uvc" => Ok(BackendKind::Uvc),
            _ => Err(BackendKindParseError {
                value: value.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendKindParseError {
    value: String,
}

impl std::fmt::Display for BackendKindParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown backend kind: {}", self.value)
    }
}

impl std::error::Error for BackendKindParseError {}
