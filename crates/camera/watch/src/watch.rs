//! The watcher: one queue of events from the three sources, read by [`Watch::poll`].

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use styx_lemnos::Client;
use styx_select::Backend;
use styx_select::listing::LibcameraLister;
use styx_select::real::Libcamera;

use crate::event::{Camera, Event};
use crate::native::Native;
use crate::queue::Queue;
use crate::registry::{Listing, Registry};
use crate::sysfs;
use crate::v4l2::V4l2;

/// Events the queue holds. The sources are read only while it has room, so a slow consumer
/// slows the kernel's and Lemnos's queues rather than losing events; [`Watch::lost`] counts any
/// that were refused anyway.
const CAPACITY: usize = 256;

/// How long the native client is pumped at a time, so the V4L2 socket and retries are looked at
/// at least this often.
const NATIVE_SLICE: Duration = Duration::from_millis(20);

/// Where the watcher reads from.
#[derive(Debug)]
pub struct WatchConfig {
    /// The Lemnos client for native cameras. `None` leaves native cameras out.
    pub client: Option<Client>,
    /// `/sys/class/video4linux` on a running system.
    pub class_dir: PathBuf,
    /// `/dev` on a running system.
    pub dev_dir: PathBuf,
    /// Whether to open the kernel's uevent socket for V4L2 nodes.
    pub netlink: bool,
    /// Whether to list libcamera's cameras once at start. The shim has no hotplug signal, so
    /// nothing after that is seen.
    pub libcamera: bool,
}

impl WatchConfig {
    /// The running system: `/sys/class/video4linux`, `/dev`, the uevent socket, and libcamera
    /// listed once.
    pub fn system(client: Option<Client>) -> Self {
        Self {
            client,
            class_dir: sysfs::CLASS_DIR.into(),
            dev_dir: sysfs::DEV_DIR.into(),
            netlink: true,
            libcamera: true,
        }
    }
}

/// One source's state, as [`Watch::status`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStatus {
    /// Events flow.
    Live,
    /// Not connected yet (the Lemnos service has not answered).
    Waiting,
    /// The last attempt failed; it is retried in `retry_in`. The source never gives up.
    Retrying { reason: String, retry_in: Duration },
    /// The source is not in use, and why.
    Absent { why: String },
}

/// Every source's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchStatus {
    pub native: SourceStatus,
    pub v4l2: SourceStatus,
    pub libcamera: SourceStatus,
}

/// The one stream of camera arrivals and departures.
#[derive(Debug)]
pub struct Watch {
    registry: Registry,
    queue: Queue<Event>,
    native: Option<Native>,
    v4l2: V4l2,
    libcamera: Option<String>,
    libcamera_wanted: bool,
}

impl Watch {
    /// Starts watching. The current cameras arrive as events on the first [`Watch::poll`]s.
    pub fn new(config: WatchConfig) -> Self {
        let mut watch = Self {
            registry: Registry::default(),
            queue: Queue::with_capacity(CAPACITY),
            native: config.client.map(Native::new),
            v4l2: V4l2::new(config.class_dir, config.dev_dir, config.netlink),
            libcamera: None,
            libcamera_wanted: config.libcamera,
        };
        if config.libcamera {
            watch.list_libcamera();
        }
        // Sysfs is read now, so the nodes that exist arrive on the first polls; the uevent socket
        // (when wanted) opens on the first poll with time for it.
        watch
            .v4l2
            .resync(&mut watch.registry, &mut watch.queue, Instant::now());
        watch
    }

    /// The next event, waiting up to `timeout` for one. `None` when nothing happened in time.
    pub fn poll(&mut self, timeout: Duration) -> Option<Event> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(event) = self.queue.pop() {
                return Some(event);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            self.step(deadline - now, now);
        }
    }

    /// The cameras present now, for a consumer that starts after some arrived.
    pub fn cameras(&self) -> &[Camera] {
        self.registry.cameras()
    }

    /// Events dropped because the consumer did not take them in time. Zero when all were taken.
    pub fn lost(&self) -> u64 {
        self.queue.refused()
    }

    /// Replays one kernel uevent as if it had been read from the socket. For tests and for
    /// replaying a recording.
    pub fn feed_uevent(&mut self, message: &[u8]) {
        self.v4l2
            .apply_message(message, &mut self.registry, &mut self.queue);
    }

    /// Every source's state now.
    pub fn status(&self) -> WatchStatus {
        let now = Instant::now();
        let native = match &self.native {
            Some(native) => native.status(now),
            None => SourceStatus::Absent {
                why: "no Lemnos client".into(),
            },
        };
        let v4l2 = if !self.v4l2.netlink() {
            SourceStatus::Absent {
                why: "uevent socket not wanted; sysfs listed once at start".into(),
            }
        } else if let Some(reason) = self.v4l2.reason() {
            SourceStatus::Retrying {
                reason: reason.to_string(),
                retry_in: self.v4l2.until_retry(now),
            }
        } else if self.v4l2.has_socket() {
            SourceStatus::Live
        } else {
            SourceStatus::Waiting
        };
        let libcamera = SourceStatus::Absent {
            why: match (&self.libcamera, self.libcamera_wanted) {
                (Some(error), _) => format!("listing at start failed: {error}"),
                (None, true) => {
                    "listed once at start: the libcamera shim has no hotplug signal".into()
                }
                (None, false) => "not wanted".into(),
            },
        };
        WatchStatus {
            native,
            v4l2,
            libcamera,
        }
    }

    /// One pass over the sources, waiting at most `left` in total.
    fn step(&mut self, left: Duration, now: Instant) {
        let Self {
            registry,
            queue,
            native,
            v4l2,
            ..
        } = self;
        v4l2.open_if_due(registry, queue, now);
        match native {
            Some(native) => {
                native.pump(left.min(NATIVE_SLICE), registry, queue, now);
                v4l2.drain(registry, queue, Instant::now());
            }
            None if v4l2.has_socket() => {
                if v4l2.ready(left) {
                    v4l2.drain(registry, queue, Instant::now());
                }
            }
            None => {
                // Nothing to wait on but a retry: sleep until it is due, or the whole wait.
                let retry = v4l2.until_retry(now);
                let nap = if retry.is_zero() {
                    left
                } else {
                    left.min(retry)
                };
                thread::sleep(nap);
            }
        }
    }

    fn list_libcamera(&mut self) {
        match Libcamera.list_libcamera() {
            Ok(listed) => {
                for camera in listed {
                    self.registry.add(
                        Listing {
                            backend: Backend::Libcamera,
                            key: camera.key,
                            anchors: camera.anchors,
                            virtual_node: camera.virtual_node,
                        },
                        &mut self.queue,
                    );
                }
            }
            Err(e) => self.libcamera = Some(e.to_string()),
        }
    }
}
