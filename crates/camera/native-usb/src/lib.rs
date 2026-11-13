//! USB cameras on Styx's native path: a `camera.frames/1` device served by Lemnos, streamed as
//! [`FrameLease`](styx_frame::FrameLease)s.
//!
//! [`UsbCamera`] finds the camera by tag and name, chooses a format ([`choose`]), selects it,
//! opens the frame stream the camera names, and starts it. Frames come through
//! [`UsbCamera::next_frame`] over the memory Lemnos filled: no copy. A camera that is unplugged,
//! fails or is restarted with its service is detached and attached again, on the backoff schedule
//! in [`Backoff`] (250 ms doubling, capped at 5 s), and nothing gives up. Stopping is manual.
//!
//! Compressed formats (MJPEG) are not chosen yet: mapping a compressed stream needs the Lemnos
//! frame's byte length, which `styx-lemnos` does not carry. Decoding is the caller's.
//!
//! Hardware facts stay in Lemnos's `uvc` driver. This crate holds no register, ioctl or USB code.

mod backoff;
mod camera;
mod choice;
mod device;

pub use backoff::{Backoff, CAP as BACKOFF_CAP, FIRST as BACKOFF_FIRST};
pub use camera::{CameraStatus, HEALTH_INTERVAL, Phase, Settings, UsbCamera};
pub use choice::{Choice, Preference, choose};
pub use device::{FindError, find_device};
pub use styx_lemnos::{Client, FRAMES_TAG};
