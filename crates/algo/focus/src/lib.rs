//! Autofocus through the Lemnos `lens/1` contract.
//!
//! [`Autofocus`] runs styx-aaa's AF (contrast detection, and PDAF where the sensor sends phase
//! data) and moves a focus lens on the Lemnos board with the lens contract's `set_position`.
//! The lens position comes back as the normalised Focus control ([`Focus::stamp`], 0 at the
//! nearest end of the lens's range, 1000 at the farthest). The AF algorithm is styx-aaa's: this
//! crate only wires it to a lens. No register, I²C or ioctl access is here; the lens driver
//! (Lemnos's `vcm` for a voice-coil motor) does that.
//!
//! `lens/1` gives the range and the position, not the direction or the settle time. The direction
//! comes from the AF dioptre map ([`Direction::of_map`]), the settle time from the config.

mod autofocus;
mod lens;

pub use autofocus::{Autofocus, AutofocusConfig, Focus};
pub use lens::{Direction, Error, FocusScale, LENS_TAG, LensDevice};
