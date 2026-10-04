//! The portable Styx camera runtime: the logic that runs a camera, written once against
//! `styx-hal` and the `styx-sensor` driver, `no_std` + `alloc` (feature `std` on Linux).
//!
//! * [`SensorState`]: the sensor side of a camera. Bring-up sequencing (power, chip id,
//!   stream-off before init for a sensor a dead owner left streaming, init, mode), serving the
//!   receiver's start and stop, frame starts driving the frame-exact control schedule, typed
//!   requests with immediate writes inside the current frame, the values that produced each
//!   frame (predicted or read back from embedded data), the lens and PDAF data, shut-down.
//! * [`Controls`]: a cloneable handle for typed, frame-accurate controls.
//! * [`LensControl`], [`PdafFrames`]: the focus lens moved frame-exactly over any
//!   [`styx_hal::LensActuator`], and phase detection data per frame.
//! * [`Health`]: the fault that ends a stream and its counters.
//! * [`Camera`] on a [`Platform`] (a [`styx_hal::Receiver`] and a [`SensorSide`]): start and
//!   stop in the order every path ends clean, a [`FrameStream`] of [`Frame`]s (leases on the
//!   receiver's buffers from a [`Pool`], given back on drop, outliving their stream), and the
//!   sensor service [`serve_sync`] (frame starts to the control schedule) for whatever waits
//!   on the receiver's frame starts.
//! * [`Frame::into_lease`]: a frame handed on as a `styx_core` [`FrameLease`](styx_core::buffer::FrameLease)
//!   over the receiver's buffer, without copying ([`FrameBacking`], described by the buffer's
//!   [`LeaseBuffer`]): the one frame type consumers see on every target.
//! * [`metrics::Counters`]: frames, drops by cause, latency and ISP time windows, the 3A
//!   loop's and AF's state, stills; relaxed atomics, no allocation (`styx::metrics` snapshots
//!   them on Linux). `styx_core::metrics`, re-exported.
//! * [`Shared`], [`Counter`]: one answer per build (`Arc<Mutex>` with `std`, `Arc<RefCell>`
//!   without), see [`sync`].
//!
//! The runtime never spawns, sleeps or blocks on its own: the platform runs its parts (on
//! threads on Linux, tasks with Embassy, one loop on a superloop). `styx-native` is the Linux
//! platform and the reference implementation. See `docs/portability-design.md`.

#![cfg_attr(not(feature = "std"), no_std)]
// Without `std`, `Ref` is an `Arc` also around state that is not `Sync` (it is then an `Rc`
// with atomic counts: it cannot leave its task); `Arc` so that what is `Sync` (the receiver,
// the frame pool, frames handed on as `FrameLease`s) can be shared (see `sync`).
#![cfg_attr(not(feature = "std"), allow(clippy::arc_with_non_send_sync))]

extern crate alloc;

mod camera;
mod controls;
mod error;
mod health;
mod lease;
mod lens;
mod sensor;
mod side;
mod stream;
pub mod sync;

pub use camera::{Camera, CameraError, CameraOptions, Platform, RunError, SensorHandle};
pub use controls::{BlankingHook, Controls};
pub use error::{Error, Result};
pub use health::{Fault, Health, MAX_CONTROL_FAILURES};
pub use lease::{FrameBacking, LeaseBuffer};
pub use lens::{LensControl, LensDrive, PdafFrames};
pub use sensor::{
    BringUpTimes, Clock, DEFAULT_WRITE_MARGIN, FrameControls, SensorState, ServeError, StartFormat,
    standby_problems,
};
pub use side::{SensorSide, SyncSource, serve_sync, sync_event};
pub use stream::{
    Frame, FrameItem, FrameStream, Lease, Pool, ReceiverError, StreamStats, instant_duration,
};
pub use styx_core;
/// The camera's counters: `styx_core::metrics` (one set from the receiver to `styx::metrics`).
pub use styx_core::metrics;
pub use styx_hal;
pub use styx_sensor;
pub use sync::{Counter, Shared};
