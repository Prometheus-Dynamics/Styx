//! Native controls: exposure, gain and frame rate posted to a `camera.sensor/1` sensor, each for
//! the frame it applies to.
//!
//! A value for frame `N` (a [`styx_controls::Update`], as the 3A loop makes them) is written during
//! frame `N - delay`, where `delay` is the sensor's delay for that control (the mode's
//! `exposure_delay`, `gain_delay` or `frame_length_delay`: frames from a write to the first frame it
//! shows on). The sensor shows a value written during frame `w` from frame `w + delay` on, so the
//! value lands on `N`. The landing frame is this crate's arithmetic from those delays; the contract
//! says `FrameValues::identity` is opaque, so it is not the landing frame. Exposure and gain usually
//! have different delays; each control is posted in its own frame, so both land on the same frame.
//! Each post is one `post_values` call; the sensor holds the group and launches it together, as its
//! contract says.
//!
//! A request that comes too late for its frame (the post frame has passed) is posted at once and
//! reports the later frame it lands on ([`Landing::late`]). Nothing is clamped silently:
//!
//! - a value outside the generic control range is [`Error::Value`] (from `styx-controls`);
//! - a value outside this sensor's own limits for the mode (its exposure range at the frame length
//!   in force, its gain codes, its frame lengths) is [`Error::Limit`], which names the limit.
//!
//! Units: exposure in microseconds becomes lines times `2^exposure_fraction_bits` with the mode's
//! line time; gain in thousandths becomes the sensor's code by the mode's gain law
//! (`GainRange::law`, the contract's, in `gain.rs`); frame rate in millihertz becomes the frame
//! length in whole lines, the frame length over the line time, clamped to the mode's
//! `frame_length_min`..=`frame_length_max` with the rate it achieves reported ([`FrameRate`]). No
//! nominal rate is kept anywhere: the mode's line time and frame lengths are the facts.
//!
//! [`NativeControls::metadata`] reports the values in force on a frame, from what was posted and
//! the delays. `camera.sensor/1` at the lock has no per-frame read-back, so nothing is read back
//! from the frame itself: reading the sensor's embedded data back, to verify the schedule, is not
//! wired yet.
//!
//! No register access, I2C or ioctls: the only calls are the sensor contract's.

mod controls;
mod error;
mod gain;
mod schedule;
mod units;

pub use controls::{FrameRate, Initial, Landing, Landings, NativeControls};
pub use error::{Error, Limit};
pub use gain::{code_to_gain, gain_to_code};
