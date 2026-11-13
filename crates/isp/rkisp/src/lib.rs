//! The Rockchip ISP (ISP21, ISP30 and ISP35) as Styx drives it: a neutral [`IspSettings`] becomes
//! a parameter buffer in the library's layout ([`Params`]), a job is posted on `camera.isp/1`
//! with its buffers ([`Isp`]), and the statistics that come back are decoded into [`Statistics`]
//! ([`statistics`]).
//!
//! Division of work, as `docs/design.md` sets it:
//!
//! - The layouts are Lemnos's: the parameter and statistics structures (`styx_lemnos::rkisp::uapi`,
//!   the `lemnos-driver-rkisp-sys` crate), the module bits, the layout numbers, the buffer sizes and
//!   the job the ISP takes ([`lemnos_contract_camera_isp::IspJob`]). Nothing here defines a struct.
//! - The values are Styx's: [`IspSettings`] (full scale 1.0) mapped to the chip's units here (black
//!   level in 12-bit codes, white balance and colour matrix in the chip's fixed point, the lens
//!   shading sectors resampled from the tuning's grid, the output gamma sampled from the tone curve).
//!
//! Bytes are written at the library's field offsets (`core::mem::offset_of!`): the structures are
//! packed and carry no byte-view trait, and this crate has no `unsafe`. Each write is in bounds by
//! construction: a buffer is the generation's size, and the offsets come from the same structures.
//!
//! Gaps, not worked around here:
//!
//! - ISP35, ISP32, ISP32 lite and ISP39: the library's module bits (`module::*`) cover ISP21 and
//!   ISP30 only, and the other generations' colour and statistics blocks have their own units that
//!   the library does not state here. [`Params::apply`] and [`statistics`] refuse them
//!   ([`Error::Unmapped`]); their layouts and sizes are the library's, and a board with one opens
//!   and refuses at its first apply.
//! - Untested on board: the ISP21 and ISP30 paths are written from the vendor driver's interface and
//!   checked against dev's buffers and statistics (`tests/`); the job path on a live ISP is not run
//!   here. Board runs are pending.

use std::fmt;

mod bytes;
mod job;
mod params;
mod stats;

pub use job::Isp;
pub use params::Params;
pub use stats::statistics;
pub use styx_lemnos::rkisp::{Generation, uapi};

/// Why a parameter buffer or a job was not built or posted.
#[derive(Debug)]
pub enum Error {
    /// The generation's values are not mapped (ISP35; see the crate documentation).
    Unmapped(Generation),
    /// The settings name lens shading the buffer cannot hold (tables past the sector count).
    Shading,
    /// The parameters are for another generation than the ISP.
    Generation { have: Generation, want: Generation },
    /// The ISP's answer, or a stream on it, could not be read or opened.
    Device(String),
    /// A frame or job could not be taken or written in its buffer.
    Buffer(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unmapped(g) => write!(
                f,
                "{g:?}: parameter values are not mapped (see the crate docs)"
            ),
            Self::Shading => write!(f, "lens shading does not fit the ISP's sectors"),
            Self::Generation { have, want } => {
                write!(f, "parameters for {want:?} given to a {have:?} ISP")
            }
            Self::Device(e) => write!(f, "camera.isp/1: {e}"),
            Self::Buffer(e) => write!(f, "ISP buffer: {e}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
