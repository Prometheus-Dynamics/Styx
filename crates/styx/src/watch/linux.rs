//! Video devices coming and going on Linux: kernel uevents (netlink, through Lemnos's
//! `lemnos_linux::uevent`) for `video4linux`, `media` and `usb` devices. Where no netlink
//! socket can be opened (some containers and sandboxes), every poll compares a listing of
//! `/dev` and `/sys/bus/usb/devices` with the previous one instead.

use crate::BackendKind;
use lemnos_linux::uevent::{Uevent, UeventRecv, UeventSocket};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::{DeviceWatchEvent, DeviceWatcher, WatchError};

const WATCHER_NAME: &str = "linux.video.uevent";
const BACKENDS: [BackendKind; 2] = [BackendKind::V4l2, BackendKind::Libcamera];

#[derive(Debug)]
enum Source {
    Uevents(UeventSocket),
    /// The relevant entries of `/dev` and `/sys/bus/usb/devices` at the last poll.
    Rescan(BTreeSet<PathBuf>),
}

/// Reports V4L2 and libcamera device changes (video and media nodes, USB devices).
/// Non-blocking: [`DeviceWatcher::poll`] returns what happened since the last poll.
#[derive(Debug)]
pub struct LinuxVideoFsWatcher {
    source: Source,
}

impl LinuxVideoFsWatcher {
    /// Listens to kernel uevents, or rescans on every poll where netlink is not available.
    pub fn new() -> Result<Self, WatchError> {
        let source = match UeventSocket::open() {
            Ok(socket) => Source::Uevents(socket),
            Err(_e) => {
                crate::trace::debug!(error = %_e, "device watch: no netlink, rescanning on poll");
                Source::Rescan(listing())
            }
        };
        Ok(Self { source })
    }

    /// Whether kernel uevents are available (otherwise every poll rescans).
    pub fn uses_uevents(&self) -> bool {
        matches!(self.source, Source::Uevents(_))
    }
}

impl DeviceWatcher for LinuxVideoFsWatcher {
    fn name(&self) -> &'static str {
        WATCHER_NAME
    }

    fn poll(&mut self) -> Result<Vec<DeviceWatchEvent>, WatchError> {
        let mut relevant = false;
        let mut paths = BTreeSet::new();
        match &mut self.source {
            Source::Uevents(socket) => loop {
                match socket.recv() {
                    Ok(Some(UeventRecv::Event(event))) => {
                        if let Some(path) = event_path(&event) {
                            relevant = true;
                            paths.insert(path);
                        }
                    }
                    // Events were lost: something may have changed.
                    Ok(Some(UeventRecv::Overflow)) => relevant = true,
                    Ok(None) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(WatchError::Io(error)),
                }
            },
            Source::Rescan(known) => {
                let now = listing();
                paths.extend(known.symmetric_difference(&now).cloned());
                relevant = !paths.is_empty();
                *known = now;
            }
        }
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
fn event_path(event: &Uevent) -> Option<PathBuf> {
    match event.subsystem()? {
        "video4linux" | "media" => Some(match event.get("DEVNAME") {
            Some(name) => Path::new("/dev").join(name),
            None => sys_path(&event.devpath),
        }),
        "usb" => Some(sys_path(&event.devpath)),
        _ => None,
    }
}

fn sys_path(devpath: &str) -> PathBuf {
    Path::new("/sys").join(devpath.trim_start_matches('/'))
}

/// Video and media nodes in `/dev` and the USB devices, for the rescan fallback.
fn listing() -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut add = |dir: &str, keep: fn(&str) -> bool| {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_str().is_some_and(keep) {
                out.insert(entry.path());
            }
        }
    };
    add("/dev", |name| {
        name.starts_with("video") || name.starts_with("media")
    });
    add("/sys/bus/usb/devices", |name| !name.starts_with('.'));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uevent(msg: &[u8]) -> Uevent {
        Uevent::parse(msg).expect("uevent")
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
