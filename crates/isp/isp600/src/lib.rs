//! The ISP600 as a Styx ISP family (`isp.isp600/1` under `camera.isp/1`), for the part of the
//! block that has an open source.
//!
//! # What the hardware ISP does here, and what it does not
//!
//! Hardware ISP tuning is unavailable on this block. The Lemnos library's `tuning` module writes
//! only the black level's enables and bit depths, the module enables and the temporal denoise's
//! record enable. Every other setting (white balance, digital and deflicker gain, the colour
//! matrix, gamma and tone, lens shading, denoise strength, sharpening, histogram weights and the
//! statistics decodes) has no open source for its register encodings, so this crate refuses it
//! with [`gaps::Unavailable`], whose message is "hardware ISP tuning unavailable: no open source
//! for the encodings", naming the setting. Nothing is guessed, written as a default, or dropped
//! silently. No statistics are decoded: the controller must take its statistics from the
//! software path.
//!
//! The default image path for this board family is therefore raw from the receiver into the
//! software ISP (`styx-soft`) or the GPU ISP (`styx-gpu`), with Styx's own 3A. The hardware ISP
//! is optional, and is useful here only for the black level and the enables.
//!
//! # What the crate holds
//!
//! The Lemnos library holds the block's facts: the load image and statistics buffer regions, the
//! register offsets and module bits the vendor kernel names, the address and extent words, the
//! framing checks, and the tuning writers ([`library`], re-exported from `styx-lemnos` with its
//! `isp600` feature). Styx holds the values, and only the values the library can encode.
//!
//! - [`layout`]: the contract's numbers, `isp.isp600/1` [`Isp600Info`](layout::Isp600Info) and its
//!   regions, answered from the library's own tables.
//! - [`load`]: the load image for the supported settings: the black level and bit depths, the
//!   BLC module enable and the temporal denoise record enable, written with the library's tuning
//!   writers into a zeroed image of the library's size. Every address and extent word stays zero,
//!   and the image passes the library's `check_load` before anything would hand it to the kernel.
//!   A non-zero extent or address word is refused.
//! - [`gaps`]: the unavailable settings, one error each, and the blocks behind them.
//! - [`stats`]: the statistics buffer read by region. The regions are views, not decodes.
//! - [`job`]: the `camera.isp/1` job posted on the config stream, encoded with the contract's
//!   codec.
//!
//! # Black level
//!
//! The public headers give the black level's enable per channel and its bit depths, but not the
//! amount, and no map from the Bayer channels to the block's three channels. So a non-zero black
//! level enables all three channels together and enables the BLC module, and the block applies
//! the offset it already holds. Until the offset's encoding is open, the black level's amount is
//! not controlled by this crate; the software path applies the black level itself.
//!
//! Hardware status: untested on board. The goldens and layouts are the library's own output; no
//! ISP600 code in `dev` is compared against (`crates/pipeline/src/chips.rs` marks it missing).

#![forbid(unsafe_code)]

/// The Lemnos library: layouts, regions, bits, framing checks and the tuning writers, unchanged.
pub use styx_lemnos::isp600 as library;

pub mod gaps;
pub mod job;
pub mod layout;
pub mod load;
pub mod stats;
