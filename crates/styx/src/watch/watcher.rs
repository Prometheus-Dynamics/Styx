use crate::BackendKind;
use std::path::PathBuf;

use super::WatchError;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceWatchEvent {
    pub watcher: &'static str,
    pub backends: Vec<BackendKind>,
    pub paths: Vec<PathBuf>,
}

impl DeviceWatchEvent {
    pub fn new(watcher: &'static str, backends: Vec<BackendKind>, paths: Vec<PathBuf>) -> Self {
        Self {
            watcher,
            backends,
            paths,
        }
    }

    pub fn touches(&self, backend: BackendKind) -> bool {
        self.backends.contains(&backend)
    }
}

pub trait DeviceWatcher: Send {
    fn name(&self) -> &'static str;
    fn poll(&mut self) -> Result<Vec<DeviceWatchEvent>, WatchError>;
}

#[derive(Default)]
pub struct CompositeWatcher {
    watchers: Vec<Box<dyn DeviceWatcher>>,
}

impl CompositeWatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, watcher: impl DeviceWatcher + 'static) {
        self.watchers.push(Box::new(watcher));
    }

    pub fn is_empty(&self) -> bool {
        self.watchers.is_empty()
    }
}

impl DeviceWatcher for CompositeWatcher {
    fn name(&self) -> &'static str {
        "styx.composite"
    }

    fn poll(&mut self) -> Result<Vec<DeviceWatchEvent>, WatchError> {
        let mut events = Vec::new();
        for watcher in &mut self.watchers {
            events.extend(watcher.poll()?);
        }
        Ok(events)
    }
}
