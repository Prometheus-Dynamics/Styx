use libcamera::camera_manager::HotplugEvent;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};

use crate::BackendKind;

use super::{DeviceWatchEvent, DeviceWatcher, WatchError};

const WATCHER_NAME: &str = "libcamera.hotplug";

#[derive(Debug)]
pub struct LibcameraHotplugWatcher {
    receiver: Receiver<HotplugEvent>,
}

impl LibcameraHotplugWatcher {
    pub fn new() -> Result<Self, WatchError> {
        let receiver = styx_libcamera::subscribe_hotplug_events().map_err(WatchError::Backend)?;
        Ok(Self { receiver })
    }
}

impl DeviceWatcher for LibcameraHotplugWatcher {
    fn name(&self) -> &'static str {
        WATCHER_NAME
    }

    fn poll(&mut self) -> Result<Vec<DeviceWatchEvent>, WatchError> {
        let mut paths = Vec::new();
        loop {
            match self.receiver.try_recv() {
                Ok(HotplugEvent::Added(id)) | Ok(HotplugEvent::Removed(id)) => {
                    paths.push(PathBuf::from(id));
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(WatchError::Backend(
                        "libcamera hotplug channel disconnected".to_string(),
                    ));
                }
            }
        }

        if paths.is_empty() {
            return Ok(Vec::new());
        }

        Ok(vec![DeviceWatchEvent::new(
            self.name(),
            vec![BackendKind::Libcamera],
            paths,
        )])
    }
}
