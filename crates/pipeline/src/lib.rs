//! The native processing pipeline: raw frames from a sensor Styx drives itself, a 3A loop
//! closed over the sensor's frame-exact controls, and an ISP turning the raw frames into RGB,
//! NV12 or luma.
//!
//! ```text
//!             ┌──────────── SensorRequest (lands on frame F + issue latency + delay) ───────────┐
//!             ▼                                                                                 │
//! sensor ─ raw frame F ─► ISP ─► NV12 / RGB / luma                                              │
//!   (exposure, gain,       │ statistics of F                                                    │
//!    frame duration of F,  ▼                                                                    │
//!    read back from     stats::from_pisp / stats::from_softisp ─► Controller (styx-algo) ───────┘
//!    embedded data)                                                     │
//!                                                                       └─► IspSettings (gains, CCM, gamma,
//!                                                                           black level, lens shading)
//! ```
//!
//! * [`SensorInfo`]: the sensor mode as the algorithms see it, from a `styx-sensor`
//!   description (limits, control delays, black level, colour filter).
//! * [`stats`]: PiSP front end statistics and software ISP statistics → [`styx_algo::Statistics`].
//! * [`Controller`]: the deterministic 3A loop: statistics plus what produced the frame in,
//!   [`SensorRequest`](styx_algo::SensorRequest)s and [`IspSettings`] out; records
//!   `styx-algo` replays.
//! * [`IspSettings`]: the ISP part of the algorithms' output, mapped onto the software ISP
//!   ([`IspSettings::softisp`]) or the PiSP back and front ends ([`IspSettings::apply_be`],
//!   [`IspSettings::apply_fe`]).
//! * [`SoftLoop`]: the loop with `styx-softisp` doing the processing and the statistics
//!   (statistics of frame F set up the ISP for frame F + 1).
//! * [`rawrec`]: raw recordings (frames plus what produced them) and [`replay`]: a virtual
//!   sensor that replays a recording at whatever exposure and gain the loop asks for, with
//!   the sensor's control delays, so the closed loop runs on a host.
//! * `device` (feature `device`): the loop on a native camera, with the PiSP
//!   ([`device::PispPipeline`]: front end statistics, back end with dma-buf hand-off, two
//!   outputs) or the software ISP ([`device::SoftPipeline`]).
//!
//! See `docs/native-stack/pipeline.md`.

pub mod controller;
#[cfg(feature = "device")]
pub mod device;
mod error;
pub mod isp;
pub mod measure;
pub mod rawrec;
pub mod replay;
pub mod sensor;
pub mod soft;
pub mod stats;

pub use controller::{Controller, SensorValues, Step};
pub use error::{PipelineError, Result};
pub use isp::IspSettings;
pub use sensor::SensorInfo;
pub use soft::{SoftLoop, SoftOutput};
pub use styx_algo;
