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
//! reports the later frame it lands on ([`Landing::late`]). Nothing is refused for a value the
//! sensor cannot take exactly, and nothing is fitted here: the value is posted as asked, the driver
//! clamps it to what it can take, and its report says what it applied and that it clamped
//! ([`NativeControls::observe`], read at each frame start from the sensor's `in_force`). The clamp
//! and the applied value shown by [`NativeControls::exposure_report`] and
//! [`NativeControls::clamp`] are the driver's; the reason (ceiling or floor) is read off the
//! requested and applied values. A driver that answers `NOT_SUPPORTED` to `in_force` gives no
//! report: the applied values are then unknown, and nothing is clamped by Styx.
//!
//! A report has no frame in the contract: it is what the driver holds now, and a value posted in
//! frame `w` shows from `w + delay`. So a report is kept for a frame only when the frame's value is
//! known: every post of that control has landed by the frame. The landing comes from the sensor
//! (`post_values_at` answers the frame the group launched in, which is the receiver's count and may
//! be past the session's frame). A value still on its way is unknown until it lands, and so is
//! every report once a sensor has refused a landing (`NOT_SUPPORTED`) for a post.
//!
////! Refused, with nothing posted: a value outside the generic control range ([`Error::Value`], from
//! `styx-controls`), a control this path does not post, automatic values, a zero frame rate, and a
//! mode that cannot take controls.
//!
//! Units: exposure in microseconds becomes lines times `2^exposure_fraction_bits` with the mode's
//! line time; gain in thousandths becomes the sensor's code by the mode's gain law
//! (`GainRange::law`, the contract's, in `gain.rs`); frame rate in millihertz becomes the frame
//! length in whole lines, the frame length over the line time. The sensor's limits are the driver's
//! to apply; [`FrameRate`] carries the driver's report. No nominal rate is kept anywhere.
//!
//! [`NativeControls::metadata`] reports the values in force on a frame: the driver's report where it
//! is known for that frame, and otherwise what was posted and landed, at the sensor's landing.
//! `camera.sensor/1` at the lock has no per-frame read-back, so nothing is read back from the frame
//! itself: reading the sensor's embedded data back, to verify the schedule, is not wired yet.
//!
//! No register access, I2C or ioctls: the only calls are the sensor contract's.

mod clamp;
mod controls;
mod error;
mod gain;
mod schedule;
mod units;

pub use clamp::{Clamp, ClampReason, ExposureReport};
pub use controls::{FrameRate, Initial, Landing, Landings, NativeControls};
pub use error::{Error, Limit};
pub use gain::{code_to_gain, gain_to_code};
