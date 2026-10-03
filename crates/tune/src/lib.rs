//! Camera calibration for Styx, the counterpart of Raspberry Pi's Camera Tuning Tool (`ctt`)
//! in Rust: raw captures of standard targets in, a tuning Styx loads out.
//!
//! * Inputs ([`input`]): Styx raw recordings (`.jsonl` + `.raw`), Styx MCAP recordings
//!   (feature `mcap`, frames with their exposure and gains) and DNG files; a [`session`] says
//!   what each capture shows (dark, flat field at a colour temperature, ColorChecker at a
//!   temperature and optionally a lux level, grey card, static scene).
//! * Calibration ([`calib`]): black level per channel and gain, hot pixels; lens shading
//!   tables per colour temperature; the AWB colour temperature curve; colour matrices per
//!   temperature (chart found automatically, [`chart`], or from corners given by hand); the
//!   noise profile; the lux reference; green equalisation.
//! * Output: a [`Tuning`] (styx-algo's model), written as Styx TOML or a Raspberry Pi tuning
//!   file (`Tuning::to_rpi_json_string`), and a [`report`].
//! * [`synth`]: a synthetic sensor with known response, shading and noise, to check the
//!   calibration gives back what the frames were made with.
//!
//! ```no_run
//! use std::path::Path;
//! use styx_tune::{calib, input::Loader, session::SessionFile};
//!
//! let dir = Path::new("captures");
//! let session = SessionFile::from_dir(dir)?;
//! let shots = session.load(dir, &Loader::new())?;
//! let cal = calib::calibrate(&shots, &styx_tune::Tuning::default(), &calib::Options::default())?;
//! std::fs::write("sensor.toml", cal.tuning.to_toml_string()?).unwrap();
//! print!("{}", styx_tune::report::text(&cal));
//! # Ok::<(), styx_tune::TuneError>(())
//! ```
//!
//! See `docs/tuning.md` for what to shoot and how Styx picks the result up.

pub mod calib;
pub mod chart;
pub mod colour;
mod error;
pub mod input;
pub mod linalg;
pub mod raw;
pub mod report;
pub mod session;
pub mod synth;

pub use error::{Result, TuneError};
pub use styx_algo::Tuning;
