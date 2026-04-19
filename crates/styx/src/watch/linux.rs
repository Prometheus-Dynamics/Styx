use crate::BackendKind;
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::{DeviceWatchEvent, DeviceWatcher, WatchError};

const WATCHER_NAME: &str = "linux.video.fs";
const DEFAULT_EVENT_BUFFER_SIZE: usize = 16 * 1024;
const WATCH_MASK: WatchMask = WatchMask::CREATE
    .union(WatchMask::DELETE)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::DELETE_SELF)
    .union(WatchMask::MOVE_SELF);

#[derive(Debug, Clone, PartialEq, Eq)]
struct WatchRegistration {
    path: PathBuf,
    backends: Vec<BackendKind>,
}

#[derive(Debug)]
pub struct LinuxVideoFsWatcher {
    inotify: Inotify,
    buffer: Vec<u8>,
    watched_paths: BTreeMap<PathBuf, WatchDescriptor>,
    registrations: BTreeMap<WatchDescriptor, WatchRegistration>,
}

impl LinuxVideoFsWatcher {
    pub fn new() -> Result<Self, WatchError> {
        let inotify = Inotify::init()?;
        let mut watcher = Self {
            inotify,
            buffer: vec![0; DEFAULT_EVENT_BUFFER_SIZE],
            watched_paths: BTreeMap::new(),
            registrations: BTreeMap::new(),
        };

        watcher.add_watch(
            PathBuf::from("/dev"),
            vec![BackendKind::V4l2, BackendKind::Libcamera],
        )?;
        watcher.add_watch(
            PathBuf::from("/sys/class/video4linux"),
            vec![BackendKind::V4l2, BackendKind::Libcamera],
        )?;
        watcher.add_watch(
            PathBuf::from("/sys/bus/usb/devices"),
            vec![BackendKind::V4l2, BackendKind::Libcamera],
        )?;
        watcher.add_watch(
            PathBuf::from("/dev/v4l/by-id"),
            vec![BackendKind::V4l2, BackendKind::Libcamera],
        )?;

        Ok(watcher)
    }

    fn add_watch(&mut self, path: PathBuf, backends: Vec<BackendKind>) -> Result<(), WatchError> {
        if self.watched_paths.contains_key(&path) || !path.exists() {
            return Ok(());
        }

        let descriptor = self.inotify.watches().add(&path, WATCH_MASK)?;
        let registration = WatchRegistration {
            path: path.clone(),
            backends,
        };
        self.watched_paths.insert(path, descriptor.clone());
        self.registrations.insert(descriptor, registration);
        Ok(())
    }

    fn registration_for(&self, descriptor: &WatchDescriptor) -> Option<&WatchRegistration> {
        self.registrations.get(descriptor)
    }
}

impl DeviceWatcher for LinuxVideoFsWatcher {
    fn name(&self) -> &'static str {
        WATCHER_NAME
    }

    fn poll(&mut self) -> Result<Vec<DeviceWatchEvent>, WatchError> {
        let events = match self.inotify.read_events(&mut self.buffer) {
            Ok(events) => events.map(|event| event.to_owned()).collect::<Vec<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(Vec::new()),
            Err(error) => return Err(WatchError::Io(error)),
        };

        if events.is_empty() {
            return Ok(Vec::new());
        }

        let mut backends = BTreeSet::new();
        let mut paths = BTreeSet::new();

        for event in events {
            if event.mask.contains(EventMask::Q_OVERFLOW) {
                backends.insert(BackendKind::V4l2);
                backends.insert(BackendKind::Libcamera);
                continue;
            }

            let Some(registration) = self.registration_for(&event.wd) else {
                continue;
            };
            if !is_relevant_event(registration, &event) {
                continue;
            }

            backends.extend(registration.backends.iter().copied());
            paths.insert(event_path(&registration.path, event.name.as_deref()));
        }

        if backends.is_empty() && paths.is_empty() {
            return Ok(Vec::new());
        }

        Ok(vec![DeviceWatchEvent::new(
            self.name(),
            backends.into_iter().collect(),
            paths.into_iter().collect(),
        )])
    }
}

fn event_path(root: &Path, name: Option<&std::ffi::OsStr>) -> PathBuf {
    match name {
        Some(name) => root.join(name),
        None => root.to_path_buf(),
    }
}

fn is_relevant_event(
    registration: &WatchRegistration,
    event: &inotify::Event<std::ffi::OsString>,
) -> bool {
    if event
        .mask
        .intersects(EventMask::DELETE_SELF | EventMask::MOVE_SELF)
    {
        return true;
    }

    let Some(name) = event.name.as_deref().and_then(|name| name.to_str()) else {
        return false;
    };

    let path = registration.path.as_path();
    if path == Path::new("/dev") {
        return name.starts_with("video") || name.starts_with("media") || name == "v4l";
    }
    if path == Path::new("/dev/v4l/by-id") {
        return true;
    }
    if path == Path::new("/sys/class/video4linux") {
        return name.starts_with("video");
    }
    if path == Path::new("/sys/bus/usb/devices") {
        return !name.starts_with('.');
    }
    true
}
