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
//! * [`Shared`], [`Counter`]: one answer per build (`Arc<Mutex>` with `std`, `Rc<RefCell>`
//!   without), see [`sync`].
//!
//! The runtime never spawns, sleeps or blocks on its own: the platform runs its parts (on
//! threads on Linux, tasks with Embassy, one loop on a superloop). `styx-native` is the Linux
//! platform and the reference implementation. See `docs/portability-design.md`.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod controls;
mod error;
mod health;
mod lens;
mod sensor;
pub mod sync;

pub use controls::{BlankingHook, Controls};
pub use error::{Error, Result};
pub use health::{Fault, Health, MAX_CONTROL_FAILURES};
pub use lens::{LensControl, LensDrive, PdafFrames};
pub use sensor::{
    BringUpTimes, Clock, DEFAULT_WRITE_MARGIN, FrameControls, SensorState, ServeError, StartFormat,
    standby_problems,
};
pub use styx_hal;
pub use styx_sensor;
pub use sync::{Counter, Shared};
