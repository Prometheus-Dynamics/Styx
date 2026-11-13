//! The 3A loop: one controller over [`styx_aaa`] for every ISP.
//!
//! Each frame, the caller gives the frame's [`Statistics`](styx_aaa::Statistics) and its
//! [`FrameMetadata`](styx_aaa::FrameMetadata) (what produced the frame, and the application's
//! controls). The [`Controller`] runs the algorithms and hands back:
//!
//! * the sensor request for a named frame, as [`styx_controls::Update`]s ([`SensorUpdates`]:
//!   exposure, analogue gain and frame rate, each applying from the frame id the request lands
//!   on), when the request is news (a repeat is not returned);
//! * the lens move, when there is one;
//! * the neutral [`IspSettings`] for the frame: full scale 1.0, hardware independent. An ISP's
//!   own mapping (one per ISP family) turns them into its registers; the mapping is not here.
//!
//! Deterministic: the same configuration, tuning, and per-frame inputs give the same outputs bit
//! for bit, so a recording replayed through `styx_aaa::replay` gives the same requests and
//! settings. Steady state reuses its buffers: the settings are copied into the ones kept from
//! the frame before.
//!
//! `no_std` + `alloc`. The `std` feature turns on every algorithm of `styx-aaa`.

#![no_std]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

mod controller;
mod error;
mod isp;
mod sensor;

pub use controller::{Controller, Start, Step};
pub use error::{ControllerError, Result};
pub use isp::IspSettings;
pub use sensor::SensorUpdates;
