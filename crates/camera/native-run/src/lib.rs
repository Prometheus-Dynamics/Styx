//! Native camera sessions on a Lemnos board.
//!
//! A [`Session`] runs one camera's native path in the order the Lemnos camera contracts set:
//! choose a sensor mode for a [`Request`] ([`select_mode`], which says why it chose it), `configure` the receiver with that
//! mode's link, open the receiver's frame stream at the stride the receiver's layout gives,
//! `start` the receiver, `start` the sensor, then drop the mode's `drop_frames` first frames. [`Session::stop`] stops the sensor, then the
//! receiver, in reverse.
//!
//! Failure rule: a camera that drops comes back. A start that fails, or a stream that stops
//! producing frames, is torn down and tried again after a backoff that doubles from
//! [`Backoff::first`] and is capped at [`Backoff::cap`] (5 to 10 s). Nothing gives up on its own
//! unless the [`Policy`] says so ([`AutoOff::AfterFailures`]). [`Session::stop`] (or a
//! [`StopHandle`] from another thread) is the manual off. [`Session::status`] shows the backoff.
//!
//! No register access, I2C or ioctls: the only hardware calls are the Lemnos contracts. Time comes
//! through [`Clock`], so tests run the backoff without waiting.

mod backoff;
mod clock;
mod frames;
mod modes;
mod session;

pub use backoff::{AutoOff, Backoff, BackoffError, Policy};
pub use clock::{Clock, SystemClock};
pub use frames::{Frames, Lease};
pub use modes::{ModeError, Reason, Request, Selection, fourcc_for, frame_end_ns, select_mode};
pub use session::{Error, Event, Session, State, Status, StopHandle};
pub use styx_lemnos::{LayoutRefusal, SharedLayout};
pub use styx_native_controls::{
    Clamp, ClampReason, ExposureReport, FrameRate, Initial, Landing, Landings,
};
