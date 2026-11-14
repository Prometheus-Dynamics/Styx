//! A simulated Styx camera on Lemnos's simulated backend.
//!
//! [`SimCamera`] runs the native path end to end in one process: `lemnosd` with simulated sensor,
//! receiver and ISP drivers (see `drivers`) on `lemnos-sim`, found by tag and link through
//! `styx-lemnos`, and frames read as `FrameLease`s. The simulated receiver makes frames but no
//! pixels; this crate renders each frame into the lease's own buffer, as raw 10-bit RGGB (`pRAA`),
//! at the exposure and gain the sensor latched on that frame. Nothing is copied.
//!
//! The sensor's mode reports delays (see [`SensorTiming`]: frames from a write to the first frame
//! it shows on, as `camera.sensor/1` says). The frame clock is lock-step: [`SimCamera::next`]
//! grants each frame and waits for it, and a value written during frame `w` shows from frame
//! `w + delay`. Controls ([`styx_controls::Update`]) given to [`SimCamera::set_control`] name the
//! frame they show on and are posted through the sensor's contract the delay before it. A caller
//! may also post to [`SimCamera::sensor`] directly, the way `styx-native-controls` does.
//!
//! Only exposure and gain change the image. Other controls are refused by [`SimCamera::set_control`].

mod camera;
mod drivers;
mod scene;
mod silicon;

pub use camera::{Error, SimCamera, SimEvent, SimFrame};
pub use drivers::call_log;
pub use scene::{FULL_SCALE, REFERENCE_EXPOSURE_US, REFERENCE_GAIN_MILLI, Scene};
pub use silicon::Fault;
pub use silicon::SensorTiming;
