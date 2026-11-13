//! Camera calibration for Styx: the job of the reference camera tuning tool (`ctt`), in Rust.
//! Raw captures of standard targets in, a tuning file Styx's tuning search finds out.
//!
//! * Inputs ([`input`]): raw captures (packed 16-bit Bayer, the size and settings stated in a
//!   [`session`] file). A [`session`] says what each capture shows (dark, flat field at a colour
//!   temperature, ColorChecker at a temperature and optionally a lux level, grey card, static
//!   scene).
//! * Calibration ([`calib`]): black level per channel and gain, hot pixels; lens shading
//!   tables per colour temperature; the AWB colour temperature curve; colour matrices per
//!   temperature (chart found automatically, [`chart`], or from corners given by hand); the
//!   noise profile; the lux reference; green equalisation.
//! * Output ([`output`]): a [`Tuning`] (styx-aaa's model) written as styx-aaa TOML into a
//!   directory the tuning search reads, and a [`report`].
//! * [`synth`]: a synthetic sensor with known response, shading and noise, to check the
//!   calibration gives back what the frames were made with.
//!
//! ```no_run
//! use std::path::Path;
//! use styx_tune::{calib, session::SessionFile};
//!
//! let session = Path::new("captures/session.toml");
//! let shots = SessionFile::read(session)?.load(session.parent().unwrap())?;
//! let cal = calib::calibrate(&shots, &styx_tune::Tuning::default(), &calib::Options::default())?;
//! styx_tune::output::write(Path::new("/etc/styx/tuning"), "sensor", &cal.tuning)?;
//! print!("{}", styx_tune::report::text(&cal));
//! # Ok::<(), styx_tune::TuneError>(())
//! ```
//!
//! The `styx-tune` binary does the same from the command line (`styx-tune calibrate`).

pub mod calib;
pub mod chart;
pub mod colour;
mod error;
pub mod input;
pub mod linalg;
pub mod output;
pub mod raw;
pub mod report;
pub mod session;
pub mod synth;

pub use error::{Result, TuneError};
pub use styx_aaa::Tuning;
