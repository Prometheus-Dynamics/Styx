//! The native source: Lemnos device events through a `Client`.
//!
//! The client's event queue is filled only while the client is pumped, so [`Native::pump`] waits
//! (for a bounded slice) and then takes the events in order. An event that a device left (`Off`
//! or `BackingOff`) takes away every camera whose chain holds that device, at once. An event
//! that a device is `Running` again re-lists the board, and the difference from the last listing
//! is pushed. A camera is listed only while every device of its chain is `Running`.
//!
//! A failed listing is retried with the capped backoff in [`crate::backoff`].

use std::time::{Duration, Instant};

use lemnos_client::DeviceState;
use styx_lemnos::{Client, DeviceEntry, FRAMES_TAG, SENSOR_TAG};
use styx_select::Anchor;

use crate::backoff::Backoff;
use crate::event::Event;
use crate::queue::Queue;
use crate::registry::{Listing, Registry};
use crate::watch::SourceStatus;

/// This many events in one pump means the client's queue may have dropped some: re-list.
const RESYNC_AT: usize = 32;

/// One camera the board lists now: its listing and the device indices its chain holds.
#[derive(Debug)]
struct Present {
    devices: Vec<u32>,
    listing: Listing,
}

#[derive(Debug)]
pub(crate) struct Native {
    client: Client,
    present: Vec<Present>,
    connected: bool,
    dirty: bool,
    backoff: Backoff,
    reason: Option<String>,
}

impl Native {
    pub(crate) fn new(client: Client) -> Self {
        Self {
            client,
            present: Vec::new(),
            connected: false,
            dirty: true,
            backoff: Backoff::default(),
            reason: None,
        }
    }

    /// Waits up to `slice` for the client's traffic, then applies what came in, in order.
    pub(crate) fn pump(
        &mut self,
        slice: Duration,
        registry: &mut Registry,
        out: &mut Queue<Event>,
        now: Instant,
    ) {
        let _ = self.client.wait_job_event(slice);
        let connected = self.client.is_connected();
        if connected != self.connected {
            self.connected = connected;
            if connected {
                self.dirty = true;
            } else {
                self.drop_all(registry, out);
            }
        }
        let mut seen = 0;
        while !out.is_full() {
            let Some(event) = self.client.next_event() else {
                break;
            };
            seen += 1;
            if event.state == DeviceState::Running {
                self.dirty = true;
            } else {
                self.device_left(event.device, registry, out);
            }
        }
        if seen >= RESYNC_AT {
            self.dirty = true;
        }
        if self.connected && self.dirty && self.backoff.due(now) {
            self.relist(registry, out, now);
        }
    }

    /// Whether the source is listing, waiting for the service, or retrying a failed listing.
    pub(crate) fn status(&self, now: Instant) -> SourceStatus {
        if !self.connected {
            return SourceStatus::Waiting;
        }
        match &self.reason {
            Some(reason) => SourceStatus::Retrying {
                reason: reason.clone(),
                retry_in: self.backoff.until(now),
            },
            None => SourceStatus::Live,
        }
    }

    fn device_left(&mut self, device: u32, registry: &mut Registry, out: &mut Queue<Event>) {
        let mut i = 0;
        while i < self.present.len() {
            if self.present[i].devices.contains(&device) {
                let gone = self.present.swap_remove(i);
                registry.remove(gone.listing.backend, &gone.listing.key, out);
            } else {
                i += 1;
            }
        }
    }

    fn drop_all(&mut self, registry: &mut Registry, out: &mut Queue<Event>) {
        for gone in std::mem::take(&mut self.present) {
            registry.remove(gone.listing.backend, &gone.listing.key, out);
        }
    }

    fn relist(&mut self, registry: &mut Registry, out: &mut Queue<Event>, now: Instant) {
        let devices = match self.client.list() {
            Ok(devices) => devices,
            Err(e) => {
                self.fail(&format!("device list: {e}"), now);
                return;
            }
        };
        self.backoff.succeeded();
        self.reason = None;
        self.dirty = false;
        let next = present_cameras(&devices);
        let old = std::mem::take(&mut self.present);
        for gone in &old {
            if !next.iter().any(|n| n.listing.key == gone.listing.key) {
                registry.remove(gone.listing.backend, &gone.listing.key, out);
            }
        }
        for arrived in &next {
            if !old.iter().any(|o| o.listing.key == arrived.listing.key) {
                registry.add(arrived.listing.clone(), out);
            }
        }
        self.present = next;
    }

    fn fail(&mut self, reason: &str, now: Instant) {
        let wait = self.backoff.failed(now);
        self.reason = Some(format!("{reason} (retry in {}s)", wait.as_secs()));
    }
}

/// The cameras a board lists with every device running: each camera chain, and each frames-only
/// device. The same cameras `styx-select`'s Lemnos lister gives, with the device indices.
fn present_cameras(devices: &[DeviceEntry]) -> Vec<Present> {
    let mut out = Vec::new();
    for camera in styx_native::cameras(devices).cameras {
        let chain: Vec<&DeviceEntry> = std::iter::once(camera.sensor())
            .chain(std::iter::once(camera.receiver()))
            .chain(camera.isps())
            .collect();
        if chain.iter().any(|d| d.state != DeviceState::Running) {
            continue;
        }
        out.push(present(
            camera.name(),
            chain.iter().map(|d| d.index).collect(),
        ));
    }
    for device in devices.iter().filter(|d| {
        d.tags.iter().any(|t| t == FRAMES_TAG)
            && !d.tags.iter().any(|t| t == SENSOR_TAG)
            && d.state == DeviceState::Running
    }) {
        out.push(present(&device.name, vec![device.index]));
    }
    out
}

fn present(name: &str, devices: Vec<u32>) -> Present {
    Present {
        devices,
        listing: Listing {
            backend: styx_select::Backend::Native,
            key: name.to_string(),
            anchors: vec![Anchor::Lemnos(name.to_string())],
            virtual_node: false,
        },
    }
}
