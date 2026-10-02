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
//! * [`Tuning`]: typed tuning, from our TOML or from Raspberry Pi tuning JSON (version 2).
//! * [`sim`]: a synthetic sensor and scene with control delays, noise and flicker, for testing
//!   convergence; [`replay`]: record and replay statistics sequences.
//!
//! ```
//! use styx_algo::{CameraConfig, FrameMetadata, Pipeline, Tuning, sim};
//!
//! let mut pipeline = Pipeline::from_tuning(&Tuning::default()).unwrap();
//! let config = CameraConfig::default();
//! pipeline.prepare(&config).unwrap();
//! let mut run = sim::Simulation::new(sim::SensorModel::default(), sim::Scene::constant(200.0, 5000.0), &config);
//! let frames = run.run(&mut pipeline, 60);
//! assert!(frames.last().unwrap().params.ae.locked);
//! ```
//!
//! See `docs/native-stack/algorithms.md` for adding an algorithm and the tuning mapping.

pub mod algos;
mod config;
mod error;
mod frame;
mod params;
mod pipeline;
pub mod pwl;
pub mod replay;
pub mod sim;
pub mod stats;
pub mod tuning;
mod warm;

pub use config::{CameraConfig, ControlDelays, Crop};
pub use error::{AlgoError, Result};
pub use frame::{Controls, Flicker, FrameMetadata, metering};
pub use params::{
    AeStatus, AwbStatus, BlackLevels, IDENTITY, LensShading, Matrix3, Params, SensorRequest,
    mat_mul,
};
pub use pipeline::{Algorithm, Pipeline};
pub use pwl::Pwl;
pub use stats::{ColourZone, Histogram, LumaZone, Statistics, StatsAccumulator, ZoneGrid};
pub use tuning::Tuning;
pub use warm::WarmStart;
