//! Video devices coming and going on Linux, through Lemnos's device watcher
//! (`lemnos_linux::device_watch`) for `video4linux`, `media` and `usb` devices: kernel uevents
//! over netlink, else inotify on sysfs and `/dev`, else a sysfs rescan on every poll (some
//! containers and sandboxes allow neither).

use crate::BackendKind;
use lemnos_linux::uevent::{UeventMsg, UeventRef};
use lemnos_linux::{DeviceWatchSource, Filter, LinuxPaths};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::{DeviceWatchEvent, DeviceWatcher, WatchError};

const WATCHER_NAME: &str = "linux.video.uevent";
const BACKENDS: [BackendKind; 2] = [BackendKind::V4l2, BackendKind::Libcamera];
const FILTER: Filter = Filter::subsystems(&["video4linux", "media", "usb"]);

/// Reports V4L2 and libcamera device changes (video and media nodes, USB devices).
/// Non-blocking: [`DeviceWatcher::poll`] returns what happened since the last poll.
#[derive(Debug)]
pub struct LinuxVideoFsWatcher {
    inner: lemnos_linux::DeviceWatcher,
}

impl LinuxVideoFsWatcher {
    /// Listens to kernel uevents (or inotify), else rescans sysfs on every poll.
    pub fn new() -> Result<Self, WatchError> {
        let inner = lemnos_linux::DeviceWatcher::new(LinuxPaths::default(), FILTER);
        crate::trace::debug!(source = ?inner.source(), "device watch");
        Ok(Self { inner })
    }

    /// Whether changes arrive on a descriptor (kernel uevents or inotify); otherwise every
    /// poll rescans sysfs.
    pub fn uses_uevents(&self) -> bool {
        self.inner.source() != DeviceWatchSource::Rescan
    }
}

impl DeviceWatcher for LinuxVideoFsWatcher {
    fn name(&self) -> &'static str {
        WATCHER_NAME
    }

    fn poll(&mut self) -> Result<Vec<DeviceWatchEvent>, WatchError> {
        let mut relevant = false;
        let mut paths = BTreeSet::new();
        self.inner
            .read(|item| match item {
                UeventRef::Event(msg) => {
                    if let Some(path) = event_path(&msg) {
                        relevant = true;
                        paths.insert(path);
                    }
                }
                // Events were lost: something may have changed.
                UeventRef::Overflow => relevant = true,
            })
            .map_err(WatchError::Io)?;
        if !relevant {
            return Ok(Vec::new());
        }
        Ok(vec![DeviceWatchEvent::new(
            self.name(),
            BACKENDS.to_vec(),
            paths.into_iter().collect(),
        )])
    }
}

/// The path a relevant uevent names (the `/dev` node, else the sysfs device), or `None` for
/// events of other subsystems.
fn event_path(event: &UeventMsg<'_>) -> Option<PathBuf> {
    match event.subsystem()? {
        "video4linux" | "media" => Some(match event.devname() {
            Some(name) => Path::new("/dev").join(name),
            None => sys_path(event.devpath()),
        }),
        "usb" => Some(sys_path(event.devpath())),
        _ => None,
    }
}

fn sys_path(devpath: &str) -> PathBuf {
    Path::new("/sys").join(devpath.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uevent(msg: &[u8]) -> UeventMsg<'_> {
        UeventMsg::parse(msg).expect("uevent")
    }

    #[test]
    fn video_media_and_usb_events_are_relevant() {
        let video = uevent(
            b"add@/devices/platform/x/video4linux/video3\0ACTION=add\0SUBSYSTEM=video4linux\0DEVNAME=video3\0",
        );
        assert_eq!(event_path(&video), Some(PathBuf::from("/dev/video3")));
        let media = uevent(b"remove@/devices/x/media1\0SUBSYSTEM=media\0DEVNAME=media1\0");
        assert_eq!(event_path(&media), Some(PathBuf::from("/dev/media1")));
        let usb = uevent(b"bind@/devices/pci0/usb3/3-1\0SUBSYSTEM=usb\0DEVTYPE=usb_device\0");
        assert_eq!(
            event_path(&usb),
            Some(PathBuf::from("/sys/devices/pci0/usb3/3-1"))
        );
        let other = uevent(b"change@/devices/virtual/net/lo\0SUBSYSTEM=net\0");
        assert_eq!(event_path(&other), None);
    }

    #[test]
    fn a_watcher_starts_and_polls_without_events() {
        let mut watcher = LinuxVideoFsWatcher::new().expect("watcher");
        // Whatever the host does meanwhile, a poll never blocks or fails.
        watcher.poll().expect("poll");
    }
}
