//! Camera control algorithms ("3A") in Rust, driven by tuning data, deterministic and
//! replayable.
//!
//! * Inputs: [`Statistics`] (per-zone colour sums, luma zones, a luma histogram, optional focus
//!   values; hardware independent, see [`stats`]) and [`FrameMetadata`] (the exposure, gain and
//!   frame duration that produced the frame, an optional lux value, and the application's
//!   [`Controls`]).
//! * Output: [`Params`]: a [`SensorRequest`] (exposure, analogue gain, frame duration and the
//!   frame they apply from), digital gain, colour gains, colour temperature, CCM, tone curve,
//!   black levels, lens-shading tables, and AE/AWB status.
//! * [`Algorithm`]: `prepare(config)` then `process(&stats, &meta, &mut params)` per frame.
//!   [`Pipeline`] runs an ordered set over one shared `Params`.
//! * [`Tuning`]: typed, per-algorithm parameters (the structs only; reading tuning files is
//!   not in this crate).
//! * [`replay`] (feature `std-core`): record and replay statistics sequences.
//!
//! # `no_std`
//!
//! Without the default `std` feature the crate is `no_std` + `alloc`. Float maths for `no_std`
//! builds comes from `styx-math` (`libm` underneath).
//! The `serde` feature derives serialisation for the parameter types; `std-core` (in `std`)
//! turns it on, with the replay format.

#![cfg_attr(not(feature = "std-core"), no_std)]

extern crate alloc;

pub mod algos;
mod config;
mod error;
mod frame;
mod math;
mod params;
mod pipeline;
pub mod pwl;
#[cfg(feature = "std-core")]
pub mod replay;
pub mod stats;
pub mod tuning;
mod warm;

pub use algos::af::{
    AfMode, AfPause, AfPauseState, AfRange, AfSpeed, AfState, AfStatus, AfWindow, LensRequest,
    LensState,
};
pub use algos::agc::deflicker::FlickerCorrection;
pub use config::{CameraConfig, ControlDelays, Crop, LensConfig};
pub use error::{AlgoError, Result};
pub use frame::{Controls, Deflicker, Flicker, FrameMetadata, MeteringRegion, metering};
pub use params::{
    AeStatus, AwbStatus, BlackLevels, CdnParams, DenoiseParams, FrameGain, GeqParams, IDENTITY,
    LensShading, Matrix3, Params, SdnParams, SensorRequest, SharpenParams, TdnParams, mat_mul,
};
pub use pipeline::{Algorithm, Pipeline};
pub use pwl::Pwl;
#[cfg(feature = "af")]
pub use stats::FocusArea;
pub use stats::{
    ColourZone, Histogram, LumaZone, PdafZone, Statistics, StatsAccumulator, ZoneGrid,
};
pub use tuning::Tuning;
pub use warm::WarmStart;
