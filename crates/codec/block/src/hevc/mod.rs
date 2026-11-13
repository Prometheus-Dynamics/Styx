//! HEVC (H.265) bitstream parsing for a stateless decoder's control structures:
//! Annex B NAL units, the VPS, SPS and PPS, and the header of an intra IDR
//! slice. No allocation, no `unsafe`.
//!
//! This is a parser, not a codec: no sample is decoded. A caller splits a byte
//! stream with [`nal_units`], removes the emulation prevention bytes of a NAL
//! unit with [`unescape`] into a buffer of its own, and parses each unit:
//!
//! ```text
//! for nal in nal_units(stream) {
//!     let n = unescape(nal.raw, &mut buf)?;
//!     match nal.unit_type {
//!         33 => sps = parse_sps(&buf[..n])?,
//!         34 => pps = parse_pps(&buf[..n])?,
//!         19 | 20 => slice = parse_slice_header(&buf[..n], nal.unit_type, &sps, &pps)?,
//!         _ => {}
//!     }
//! }
//! ```
//!
//! [`SliceHeader::data_offset`] is an RBSP offset; [`raw_index`] maps it to the
//! byte in the NAL unit as stored, which is what a driver's slice-data offset
//! counts.
//!
//! Not parsed: the VUI and the SPS and PPS extensions (range, multilayer, 3D,
//! SCC), scaling-list values, inter and non-IDR slices (their reference picture
//! sets), and tile or wavefront entry points. Each is refused or stops the
//! read where it starts; see [`Error::Unsupported`].
//!
//! # Untrusted input
//!
//! A bitstream comes from whoever feeds the decoder. Every length, count and
//! index read from it is checked against the specification's range and against
//! the arrays that hold it before it is used, and arithmetic on stream values
//! is checked: malformed data gives an [`Error`], never a panic. The tests feed
//! every truncation and single-bit corruption of a real stream's headers.
//!
//! Provenance: moved from Lemnos `drivers/hevc` (lock 04d09360), where it was the
//! `lemnos-driver-hevc` crate; the bitstream is Styx's, so the parser lives here. Syntax and
//! value ranges from ITU-T H.265 (7.3 and 7.4).

#![forbid(unsafe_code)]

mod bits;
pub mod controls;
mod ps;
mod slice;

#[cfg(test)]
mod tests;

pub use bits::{BitReader, Error, NalUnit, NalUnits, Result, nal_units, raw_index, unescape};
pub use ps::{Pps, Sps, StRps, Vps, parse_pps, parse_sps, parse_st_rps, parse_vps};
pub use slice::{IDR_N_LP, IDR_W_RADL, SLICE_I, SliceHeader, is_idr, parse_slice_header};
