//! The frame path's hook: [`Counted`] wraps the service's outlets and counts every frame that
//! reaches them, with no lock and no allocation per frame.
//!
//! The service calls each camera's outlet on the camera's thread, just before the frame is
//! published to the readers (the same thread, with no wait in between). The counters take the
//! lease's identity and timestamp and the monotonic time; the lease itself is passed on untouched.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use styx_controls::FrameMetadata;
use styx_frame::FrameLease;
use styx_service::{Outlet, Outlets};

use crate::counters::CameraCounters;

/// One camera's counters, and what was asked of it (the requested rate and why).
#[derive(Debug, Default)]
pub struct Entry {
    pub counters: CameraCounters,
    /// The requested frame rate in thousandths per second, and the reason it may differ from the
    /// achieved rate. Set by the caller that configured the camera; read by the publisher.
    pub requested: Mutex<(u32, String)>,
    /// The last achieved rate a window measured (0 until one has two frames). The requested reason
    /// is judged against it, so a window with no frames does not hide a difference.
    pub last_rate_milli: AtomicU32,
}

/// Every camera's counters, by camera id. Locked only when a camera is attached and when the
/// publisher reads; never on the frame path.
#[derive(Debug, Default)]
pub struct Registry {
    entries: Mutex<BTreeMap<String, Arc<Entry>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Registry {
    /// The entry for `camera`, made the first time.
    pub fn entry(&self, camera: &str) -> Arc<Entry> {
        let mut entries = lock(&self.entries);
        if let Some(entry) = entries.get(camera) {
            return Arc::clone(entry);
        }
        let entry = Arc::new(Entry::default());
        entries.insert(camera.to_string(), Arc::clone(&entry));
        entry
    }

    /// Records the rate `camera` was asked for, and why its achieved rate may differ.
    pub fn request(&self, camera: &str, fps_milli: u32, reason: &str) {
        *lock(&self.entry(camera).requested) = (fps_milli, reason.to_string());
    }

    /// Every camera's entry, sorted by id.
    pub fn all(&self) -> Vec<(String, Arc<Entry>)> {
        lock(&self.entries)
            .iter()
            .map(|(id, entry)| (id.clone(), Arc::clone(entry)))
            .collect()
    }
}

/// The counting outlets: the service's outlets, with every frame counted on the way.
pub struct Counted {
    inner: Arc<dyn Outlets>,
    registry: Arc<Registry>,
}

impl Counted {
    /// Counts the frames that reach `inner`.
    pub fn new(inner: Arc<dyn Outlets>) -> Self {
        Self {
            inner,
            registry: Arc::new(Registry::default()),
        }
    }

    /// The counters, shared with the publisher.
    pub fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }
}

impl Outlets for Counted {
    fn attach(&self, camera: &str) -> Box<dyn Outlet> {
        Box::new(CountedOutlet {
            entry: self.registry.entry(camera),
            inner: self.inner.attach(camera),
        })
    }
}

struct CountedOutlet {
    entry: Arc<Entry>,
    inner: Box<dyn Outlet>,
}

impl Outlet for CountedOutlet {
    fn frame(&mut self, camera: &str, lease: &FrameLease<FrameMetadata>) {
        self.entry
            .counters
            .record_frame(lease.id(), lease.timestamp_ns(), now_ns());
        self.inner.frame(camera, lease);
    }
}

/// `CLOCK_MONOTONIC` now, in nanoseconds: the clock frame timestamps are on.
pub fn now_ns() -> u64 {
    shm_ring::SampleTime::now().as_nanos()
}

impl std::fmt::Debug for Counted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Counted")
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}
