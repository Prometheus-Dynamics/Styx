//! Per-camera counters for the camera service, read by any process.
//!
//! What is counted, per camera:
//!
//! - frames delivered (the total, and the identity and timestamp of the last one);
//! - drops by reason ([`DropReason`]): pool exhausted, held buffer, ring full, backend error, and
//!   frames the stream did not deliver with no reason given (a gap in the identities);
//! - frame copies in this process (`styx_frame::copy_counters`, shared by every camera);
//! - frame age, from the capture timestamp to the publish point, over a window: min, median and
//!   max, with a fixed 32-bucket histogram;
//! - the achieved frame rate, from the window's frame timestamps, and the requested rate with its
//!   reason when the two differ;
//! - the owning backend and the chooser's reason, the device's health and reason, the 3A state
//!   (none: no 3A controller runs in the service yet), the camera's state and its backoff.
//!
//! The frame path ([`Counted`], [`CameraCounters::record_frame`]) uses atomics only: no lock, no
//! allocation. The publisher ([`Hub`]) takes the window at its own rate and writes one
//! [`Snapshot`] per camera on the counters ring ([`Publisher`]), a fixed-layout little-endian
//! record documented in [`snapshot`]. A reader in another process connects with [`Subscriber`]
//! and decodes each snapshot with [`read_snapshot`]. There is no HTTP server here.
//!
//! The service's frame path reports a dropped frame only as a gap in the identities: the stream
//! drops it, and the native session does not report why (`styx-native-run` ends a held or
//! exhausted acquire as no frame). Those reasons are counted where a caller knows them (see
//! [`CameraCounters::record_drop`]).

mod counters;
mod hub;
mod outlet;
mod publish;
pub mod snapshot;

pub use counters::{CameraCounters, Window, achieved_fps_milli, median_of};
pub use hub::{Hub, collect, copies};
pub use outlet::{Counted, Entry, Registry, now_ns};
pub use publish::{Error, Publisher, Subscriber};
pub use snapshot::{
    AGE_BUCKETS, Age, CameraState, DROP_REASONS, DropReason, Health, MAGIC, Owner, PublishState,
    SNAPSHOT_BYTES, Snapshot, SnapshotError, Text, ThreeA, VERSION, read_snapshot,
};
