//! HEVC's NAL unit header over the shared bitstream reader ([`crate::bitstream`]).

use crate::bitstream::{AnnexB, annexb_units};
pub use crate::bitstream::{BitReader, Error, Result, raw_index, unescape};

/// One NAL unit of an Annex B byte stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NalUnit<'a> {
    /// `nal_unit_type` (6 bits): 32 VPS, 33 SPS, 34 PPS, 19 and 20 IDR, and so on.
    pub unit_type: u8,
    /// `nuh_layer_id`.
    pub layer_id: u8,
    /// `nuh_temporal_id_plus1`.
    pub temporal_id_plus1: u8,
    /// The unit's bytes: its two-byte header and payload, with emulation
    /// prevention bytes still in place. No start code, no trailing zeros.
    pub raw: &'a [u8],
}

/// Iterates the NAL units of an Annex B byte stream (start codes `00 00 01`).
/// Units with fewer than two bytes are skipped.
pub fn nal_units(stream: &[u8]) -> NalUnits<'_> {
    NalUnits {
        units: annexb_units(stream),
    }
}

/// The iterator behind [`nal_units`].
#[derive(Clone, Debug)]
pub struct NalUnits<'a> {
    units: AnnexB<'a>,
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = NalUnit<'a>;

    fn next(&mut self) -> Option<NalUnit<'a>> {
        loop {
            let raw = self.units.next()?;
            let [first, second, ..] = *raw else {
                continue;
            };
            return Some(NalUnit {
                unit_type: (first >> 1) & 0x3f,
                layer_id: ((first & 1) << 5) | (second >> 3),
                temporal_id_plus1: second & 7,
                raw,
            });
        }
    }
}
