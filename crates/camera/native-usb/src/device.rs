//! Finding the USB camera among the devices the Lemnos service lists.

use std::fmt;

use lemnos_client::DeviceEntry;
use styx_lemnos::FRAMES_TAG;

/// Why no camera was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindError {
    /// No device provides `camera.frames/1`.
    NoCamera,
    /// The named device does not exist, or does not provide `camera.frames/1`.
    NotFound(String),
    /// More than one device provides `camera.frames/1` and none was named.
    Ambiguous(usize),
}

impl fmt::Display for FindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCamera => write!(f, "no {FRAMES_TAG} device"),
            Self::NotFound(name) => write!(f, "no {FRAMES_TAG} device named {name}"),
            Self::Ambiguous(n) => write!(f, "{n} {FRAMES_TAG} devices; name one"),
        }
    }
}

impl std::error::Error for FindError {}

/// The `camera.frames/1` device called `name`, or the only one when `name` is `None`.
pub fn find_device<'a>(
    devices: &'a [DeviceEntry],
    name: Option<&str>,
) -> Result<&'a DeviceEntry, FindError> {
    let frames: Vec<&DeviceEntry> = devices
        .iter()
        .filter(|d| d.tags.iter().any(|t| t == FRAMES_TAG))
        .collect();
    match name {
        Some(name) => frames
            .into_iter()
            .find(|d| d.name == name)
            .ok_or_else(|| FindError::NotFound(name.to_string())),
        None => match frames.as_slice() {
            [] => Err(FindError::NoCamera),
            [one] => Ok(one),
            many => Err(FindError::Ambiguous(many.len())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lemnos_client::{DeviceHealth, DeviceState};

    fn device(index: u32, name: &str, tags: &[&str]) -> DeviceEntry {
        DeviceEntry {
            index,
            name: name.to_string(),
            driver: String::new(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            state: DeviceState::Running,
            health: DeviceHealth::Ok,
            reason: String::new(),
            link: None,
        }
    }

    #[test]
    fn the_only_frames_device_is_found_by_tag() {
        let devices = [
            device(0, "sensor", &["camera.sensor/1"]),
            device(1, "usb0", &[FRAMES_TAG]),
        ];
        assert_eq!(find_device(&devices, None).unwrap().name, "usb0");
    }

    #[test]
    fn several_frames_devices_need_a_name() {
        let devices = [
            device(0, "usb0", &[FRAMES_TAG]),
            device(1, "usb1", &[FRAMES_TAG]),
        ];
        assert_eq!(find_device(&devices, None), Err(FindError::Ambiguous(2)));
        assert_eq!(find_device(&devices, Some("usb1")).unwrap().index, 1);
    }

    #[test]
    fn a_named_device_without_the_tag_is_not_found() {
        let devices = [device(0, "usb0", &["gpio"])];
        assert_eq!(find_device(&devices, None), Err(FindError::NoCamera));
        assert_eq!(
            find_device(&devices, Some("usb0")),
            Err(FindError::NotFound("usb0".to_string()))
        );
    }
}
