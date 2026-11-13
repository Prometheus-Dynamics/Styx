//! The publisher's side of the service: builds each camera's snapshot from its status and its
//! counters, and publishes them on the counters ring.

use std::path::Path;
use std::sync::Arc;

use styx_select::Backend;
use styx_service::slot::{CameraStatus, Health, Publish, State};

use crate::counters::{achieved_fps_milli, median_of};
use crate::outlet::{Entry, Registry, now_ns};
use crate::publish::{Error, Publisher};
use crate::snapshot::Health as SnapHealth;
use crate::snapshot::{Age, CameraState, DropReason, Owner, PublishState, Snapshot, Text, ThreeA};

/// A requested rate and an achieved one differ by more than this many parts per thousand: the
/// requested reason is then shown.
const RATE_TOLERANCE_PER_MILLE: u64 = 10;

/// Counts the process's frame copies (`styx_frame::copy_counters`): (copies, bytes).
pub fn copies() -> (u64, u64) {
    styx_frame::copy_counters()
        .iter()
        .fold((0, 0), |(c, b), counter| {
            (c + counter.copies() as u64, b + counter.bytes() as u64)
        })
}

/// Builds one camera's snapshot. Takes (and resets) the camera's frame window.
pub fn collect(seq: u64, taken_ns: u64, status: &CameraStatus, entry: &Entry) -> Snapshot {
    let counters = &entry.counters;
    let window = counters.take_window();
    let (last_id, last_ts) = counters.last();
    let achieved = achieved_fps_milli(
        window.first_id,
        window.first_ns,
        window.last_id,
        window.last_ns,
    );
    let (requested_fps, requested_reason) = {
        let requested = entry
            .requested
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (requested.0, requested.1.clone())
    };
    // The reason is shown only when the achieved rate (the last one a window measured) differs from
    // the request by more than the tolerance.
    if achieved > 0 {
        entry
            .last_rate_milli
            .store(achieved, std::sync::atomic::Ordering::Relaxed);
    }
    let known = entry
        .last_rate_milli
        .load(std::sync::atomic::Ordering::Relaxed);
    let differs = known > 0
        && requested_fps > 0
        && u64::from(known).abs_diff(u64::from(requested_fps)) * 1000
            > u64::from(requested_fps) * RATE_TOLERANCE_PER_MILLE;
    let (copies, copy_bytes) = copies();
    let median = median_of(&window.hist, window.frames, window.max_age_ns);
    let mut drops = [0_u64; crate::snapshot::DROP_REASONS];
    for reason in DropReason::ALL {
        drops[reason as usize] = counters.drops(reason);
    }
    // The service's own counts stand for the two reasons it knows: a ring that had no free
    // buffer, and the backend's failed attempts.
    drops[DropReason::RingFull as usize] = status.publish_dropped;
    drops[DropReason::BackendError as usize] = status.total_failures;
    let (health, health_reason) = match &status.health {
        Health::Unknown => (SnapHealth::Unknown, String::new()),
        Health::Healthy => (SnapHealth::Healthy, String::new()),
        Health::Degraded(why) => (SnapHealth::Degraded, why.clone()),
        Health::Failed(why) => (SnapHealth::Failed, why.clone()),
    };
    Snapshot {
        seq,
        taken_ns,
        camera: Text::new(&status.id),
        owner: match status.owner {
            None => Owner::None,
            Some(Backend::Native) => Owner::Native,
            Some(Backend::Libcamera) => Owner::Libcamera,
            Some(Backend::V4l2) => Owner::V4l2,
        },
        owner_reason: Text::new(status.reason.as_deref().unwrap_or("")),
        state: match status.state {
            State::Absent => CameraState::Absent,
            State::Streaming => CameraState::Streaming,
            State::Retrying => CameraState::Retrying,
            State::Backoff => CameraState::Backoff,
            State::Stopped => CameraState::Stopped,
            State::Off => CameraState::Off,
        },
        health,
        health_reason: Text::new(&health_reason),
        three_a: ThreeA::None,
        publish: match status.publish {
            Publish::Waiting => PublishState::Waiting,
            Publish::Publishing => PublishState::Publishing,
            Publish::NotPublishable(_) => PublishState::NotPublishable,
        },
        frames: counters.frames(),
        last_id,
        last_timestamp_ns: last_ts,
        published: status.published,
        ring_full: status.publish_dropped,
        readers: status.readers as u32,
        drops,
        copies,
        copy_bytes,
        age: Age {
            count: window.frames,
            min_ns: window.min_age_ns,
            median_ns: median,
            max_ns: window.max_age_ns,
            hist: window.hist,
        },
        achieved_fps_milli: achieved,
        window_span_ns: window.last_ns.saturating_sub(window.first_ns),
        window_frames: u32::try_from(window.frames).unwrap_or(u32::MAX),
        requested_fps_milli: requested_fps,
        requested_reason: Text::new(if differs { &requested_reason } else { "" }),
        next_delay_ms: status
            .next_delay
            .map_or(0, |d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX)),
        failures: status.failures,
        total_failures: status.total_failures,
        last_error: Text::new(status.last_error.as_deref().unwrap_or("")),
    }
}

/// The service's counters, published: the registry the counting outlets write, the snapshots
/// built from it and the service's statuses, and the counters ring they go on.
pub struct Hub {
    registry: Arc<Registry>,
    publisher: Publisher,
    seq: u64,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("publisher", &self.publisher)
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

impl Hub {
    /// A hub for the counters registry at `socket` (the counters ring's path).
    pub fn bind(registry: Arc<Registry>, socket: &Path) -> Result<Self, Error> {
        Ok(Self {
            registry,
            publisher: Publisher::bind(socket)?,
            seq: 0,
        })
    }

    /// The counters ring's path.
    pub fn socket(&self) -> &Path {
        self.publisher.path()
    }

    /// The publisher, for its counts.
    pub fn publisher(&self) -> &Publisher {
        &self.publisher
    }

    /// One pass: accept readers, build a snapshot for each camera in `statuses` (the service's),
    /// and publish them. Returns the snapshots built, in the order of `statuses`.
    pub fn tick(&mut self, statuses: &[CameraStatus]) -> Result<Vec<Snapshot>, Error> {
        self.publisher.accept()?;
        let taken = now_ns();
        let mut built = Vec::with_capacity(statuses.len());
        for status in statuses {
            self.seq += 1;
            let entry = self.registry.entry(&status.id);
            let snapshot = collect(self.seq, taken, status, &entry);
            self.publisher.publish(&snapshot)?;
            built.push(snapshot);
        }
        Ok(built)
    }
}
