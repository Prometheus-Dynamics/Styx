//! A simulated Styx camera on Lemnos's simulated backend.
//!
//! [`SimCamera`] runs the native path end to end in one process: `lemnosd` with simulated sensor,
//! receiver and ISP drivers (see `drivers`) on `lemnos-sim`, found by tag and link through
//! `styx-lemnos`, and frames read as `FrameLease`s. The simulated receiver makes frames but no
//! pixels; this crate renders each frame into the lease's own buffer, as raw 10-bit RGGB (`pRAA`),
//! at the exposure and gain in force on that frame. Nothing is copied.
//!
//! Scenes ([`Scene`]) are a raw pattern or replayed raw frames. Controls
//! ([`styx_controls::Update`]) are applied from the frame identity they name: frame `n` is the
//! first rendered frame at or after the update's frame, and the update is refused once a frame at
//! or after it has been rendered.
//!
//! Only exposure and gain change the image. The other controls are recorded in each frame's
//! metadata and the sensor's contract, and nothing else.

mod camera;
mod drivers;
mod scene;

pub use camera::{Error, SimCamera, SimFrame};
pub use drivers::call_log;
pub use scene::{FULL_SCALE, REFERENCE_EXPOSURE_US, REFERENCE_GAIN_MILLI, Scene};
