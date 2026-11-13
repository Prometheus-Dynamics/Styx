//! H.264 (AVC) bitstream parsing for a stateless decoder's control structures: the NAL unit
//! header, the SPS and PPS, and the header of a frame's slice (ITU-T H.264, 7.3 and 7.4), with
//! the picture order count (8.2.1) and the reference picture tracker (8.2.4 and 8.2.5) that a
//! stateless decoder needs for each slice. No sample is decoded. No allocation in the parsers
//! beyond the scaling-list and offset tables of a parameter set, and no `unsafe`.
//!
//! A caller splits the byte stream with [`nal_units`], unescapes each unit into a buffer of
//! its own, and parses it:
//!
//! ```text
//! for nal in nal_units(stream) {
//!     let n = unescape(nal.raw, &mut buf)?;
//!     match nal.unit_type {
//!         7 => sps.push(parse_sps(&buf[..n])?),
//!         8 => pps.push(parse_pps(&buf[..n], |id| sps_chroma_format(id))?),
//!         1 | 5 => {
//!             let id = peek_pps_id(&buf[..n])?;
//!             let pps = find_pps(id);
//!             let sps = find_sps(pps.sps_id);
//!             let header = parse_slice_header(&buf[..n], nal.unit_type, nal.nal_ref_idc, sps, pps)?;
//!         }
//!         _ => {}
//!     }
//! }
//! ```
//!
//! Frames only: a field picture ([`SliceHeader::field_pic`]) is refused as unsupported, and so is
//! a slice group map (FMO) and a gap in `frame_num`. Their syntax is parsed as far as the
//! refusal allows. MBAFF frames are frames and parse as such.
//!
//! Not parsed: the VUI and the SPS and PPS extensions (their contents are skipped, not read), SVC
//! and MVC (`nal_unit_type` 14, 15, 20), and the prefix NAL units. The decoder is
//! [`decoder::H264Decoder`]; the layout of the block's controls is the caller's ([`controls`]),
//! since the V4L2 H.264 control structures are not at Lemnos's lock.
//!
//! Provenance: written from ITU-T H.264 (sections 7.3, 7.4, 8.2) for Styx. Lemnos has no H.264
//! parser at its lock 04d09360. No FFmpeg, GStreamer or kernel source was read or copied.

pub mod controls;
pub mod decoder;
pub mod dpb;
pub mod poc;
pub mod ps;
pub mod slice;
pub mod work;

#[cfg(test)]
mod tests;

pub use crate::bitstream::{BitReader, Error, Result, unescape};
pub use controls::{Controls, Layout, SliceView};
pub use decoder::{H264Decoder, candidate, descriptor};
pub use dpb::{Dpb, NONE, RefLists};
pub use poc::{Poc, PocState};
pub use ps::{Pps, ScalingLists, Sps, parse_pps, parse_sps, peek_pps_id};
pub use slice::{Mmco, RplMod, SliceHeader, SliceKind, WeightEntry, parse_slice_header};

use crate::bitstream::annexb_units;

/// One NAL unit of an Annex B byte stream, with the one-byte H.264 header read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NalUnit<'a> {
    /// `nal_ref_idc` (2 bits): nonzero for a reference picture.
    pub nal_ref_idc: u8,
    /// `nal_unit_type` (5 bits): 1 non-IDR slice, 5 IDR slice, 7 SPS, 8 PPS, and so on.
    pub unit_type: u8,
    /// The unit's bytes, header included, with emulation prevention bytes still in place.
    pub raw: &'a [u8],
}

/// Iterates the NAL units of an Annex B byte stream. A unit whose `forbidden_zero_bit` is set
/// is passed on: the parsers refuse it.
pub fn nal_units(stream: &[u8]) -> impl Iterator<Item = NalUnit<'_>> {
    annexb_units(stream).map(|raw| NalUnit {
        nal_ref_idc: (raw[0] >> 5) & 3,
        unit_type: raw[0] & 0x1f,
        raw,
    })
}
