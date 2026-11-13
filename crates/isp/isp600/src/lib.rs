//! The ISP600 as a Styx ISP family (`isp.isp600/1` under `camera.isp/1`).
//!
//! Lemnos owns the block's facts: the load image and statistics buffer regions, the register
//! offsets and module bits the vendor kernel names, the address words, and the framing checks
//! ([`library`], re-exported from `styx-lemnos` with its `isp600` feature). Styx owns the values:
//! what each setting asks of the block, once it is encoded. This crate does three things with
//! that split.
//!
//! - [`layout`]: the contract's numbers, `isp.isp600/1` [`Isp600Info`](layout::Isp600Info) and its
//!   regions, answered from the library's own tables.
//! - [`load`]: a zeroed load image of the library's exact size, written at the library's offsets,
//!   and passed through the library's [`check_load`](library::layout::check_load) before anything
//!   would hand it to the kernel. Address words are never set here.
//! - [`stats`] and [`job`]: the statistics buffer read by region, and the `camera.isp/1` job
//!   posted on the config stream, encoded with the contract's codec.
//!
//! [`gaps`] lists the settings this crate cannot encode yet, from the neutral
//! [`IspSettings`](styx_settings::IspSettings), so a caller learns what would be guessed. The
//! library has the regions and bits but not the register encodings of gains, colour matrix, tone
//! curve, shading, denoise or sharpening, and it does not decode statistics fields; those are
//! gaps, named per block in [`gaps`], not written here.
//!
//! Hardware status: untested on board. `dev` has no ISP600 code to compare against
//! (`crates/pipeline/src/chips.rs` marks it missing), so the goldens are the library's own output.

#![forbid(unsafe_code)]

/// The Lemnos library: layouts, regions, bits and framing checks, unchanged.
pub use styx_lemnos::isp600 as library;

pub mod gaps;
pub mod job;
pub mod layout;
pub mod load;
pub mod stats;
