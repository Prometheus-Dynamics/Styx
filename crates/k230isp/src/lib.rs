//! Canaan K230D's VeriSilicon ISP8000, driven from Rust: the ISP behind the vendor kernel's
//! `vvcam-isp` misc device (`/dev/vvcam-isp.0`), with Styx's own 3A in place of Canaan's SDK.
//!
//! - [`regs`]: the register map (Lemnos's `lemnos-drivers-k230isp`): block bases, register
//!   offsets, the field positions and the sources they come from.
//! - [`Params`]: one frame's 3A settings as an ordered list of register writes: the
//!   statistics Styx's 3A reads (`measure`: the AE grid's 5x5 luma means, the 16-bin
//!   histogram, the AWB means), black level, white balance gains, the colour matrix and its
//!   offsets, the output gamma, lens shading. [`Params::apply`] writes them through any
//!   [`RegisterAccess`].
//! - [`stats`]: the statistics registers read into [`stats::Raw`] (Lemnos's) and decoded as
//!   [`styx_algo::Statistics`] ([`stats::statistics`]).
//! - `device` (feature `device`): the ioctls of `/dev/vvcam-isp.0`: register reads and
//!   writes, and the [`RegisterAccess`] over them; Lemnos's, re-exported. Linux only.
//!
//! Status: built from the headers and unit-tested against a fake register file; not run on a
//! K230D (written, untested on board). The register layouts agree with each other across the
//! struct and the field comments, except where the crate says otherwise. The gap is that
//! the K230's own blocks (the front end at `0x3D60`, the FUSA blocks) have no field layouts
//! in the sources here, so they are left out; see `docs/k230.md`. The unconfirmed constants
//! are named where they are defined.

#![no_std]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "device")]
pub use lemnos_drivers_k230isp::device;
pub use lemnos_drivers_k230isp::{RegisterAccess, regs};
pub mod stats;

mod params;

pub use params::{
    BLS_MAX, BLS_MIN, CCM_MAX, CCM_MIN, CCM_ONE, CT_OFFSET_MAX, CT_OFFSET_MIN, GAIN_MAX, GAIN_ONE,
    GAMMA_OUT_MAX, GAMMA_POINTS, HIST_STEPSIZE, LSC_COLS, LSC_MAX, LSC_ONE, LSC_ROWS, LSC_SECTORS,
    LSC_WORDS_PER_ROW, Params, RegWrite,
};

#[cfg(test)]
mod tests;
