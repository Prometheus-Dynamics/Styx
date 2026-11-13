//! Native controls: exposure, gain and frame rate posted to a `camera.sensor/1` sensor, each for
//! the frame it applies to.
//!
//! A value for frame `N` (a [`styx_controls::Update`], as the 3A loop makes them) is written during
//! frame `N - delay`, where `delay` is the sensor's delay for that control (the mode's
//! `exposure_delay`, `gain_delay` or `frame_length_delay`). The sensor shows a value written during
//! frame `w` from frame `w + delay` on, so the value lands on `N`. Exposure and gain usually have
//! different delays; each control is posted in its own frame, so both land on the same frame. Each
//! post is one `post_values` call; the sensor holds the group and launches it together, as its
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
//! line time; gain in thousandths becomes the sensor's code with [`GainRange::unity`] as the code
//! for 1.0x (a linear model: code = unity * gain, moved in `step`s); frame rate in millihertz
//! becomes the frame length in lines. The chip facts are the mode's, from Lemnos.
//!
//! [`NativeControls::metadata`] reports the values in force on a frame, from what was posted and
//! the delays. `camera.sensor/1` does not report per-frame values, so nothing is read back from the
//! frame itself.
//!
//! No register access, I2C or ioctls: the only calls are the sensor contract's.

mod controls;
mod error;
mod schedule;
mod units;

pub use controls::{Initial, Landing, Landings, NativeControls};
pub use error::{Error, Limit};
