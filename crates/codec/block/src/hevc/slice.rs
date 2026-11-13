//! The header of an intra slice segment (7.3.6.1), for IDR pictures.
//!
//! Inter slices (P and B) and non-IDR intra pictures need the reference picture
//! set, the picture order count, and the weighted-prediction table, none of
//! which is read here. They are refused as unsupported; a decoder's
//! IDR-only streams need nothing more.

use crate::hevc::bits::{BitReader, Error, Result};
use crate::hevc::ps::{Pps, Sps};

/// `slice_type` for an I slice.
pub const SLICE_I: u32 = 2;
/// `nal_unit_type` of an IDR picture without leading pictures.
pub const IDR_W_RADL: u8 = 19;
/// `nal_unit_type` of an IDR picture without leading pictures or RADL pictures.
pub const IDR_N_LP: u8 = 20;

/// The fields of an intra slice segment header a decoder needs. Entry point
/// offsets (tiles or wavefronts) are refused, since their values are not kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SliceHeader {
    /// `first_slice_segment_in_pic_flag`.
    pub first_slice_segment_in_pic: bool,
    /// `no_output_of_prior_pics_flag` (IRAP pictures).
    pub no_output_of_prior_pics: bool,
    /// `slice_pic_parameter_set_id`.
    pub pps_id: u8,
    /// `dependent_slice_segment_flag`. A dependent segment takes the
    /// independent fields of the segment before it.
    pub dependent: bool,
    /// `slice_segment_address`, in CTBs (0 for the first slice).
    pub segment_address: u32,
    /// `slice_type` (2 for I).
    pub slice_type: u32,
    /// `pic_output_flag` (1 when the PPS does not signal it).
    pub pic_output: bool,
    /// `colour_plane_id`.
    pub colour_plane_id: u8,
    /// `slice_sao_luma_flag`.
    pub sao_luma: bool,
    /// `slice_sao_chroma_flag`.
    pub sao_chroma: bool,
    /// `slice_qp_delta`.
    pub qp_delta: i32,
    /// `slice_cb_qp_offset` (0 without the PPS's chroma offsets).
    pub cb_qp_offset: i32,
    /// `slice_cr_qp_offset` (0 without the PPS's chroma offsets).
    pub cr_qp_offset: i32,
    /// `slice_deblocking_filter_disabled_flag` (the PPS's value when not overridden).
    pub deblocking_disabled: bool,
    /// `slice_beta_offset_div2`.
    pub beta_offset_div2: i32,
    /// `slice_tc_offset_div2`.
    pub tc_offset_div2: i32,
    /// `slice_loop_filter_across_slices_enabled_flag`.
    pub loop_filter_across_slices: bool,
    /// `num_entry_point_offsets` (always 0 here: nonzero is refused).
    pub num_entry_point_offsets: u32,
    /// The byte offset of the slice data in the RBSP (header included), which
    /// is where `byte_alignment()` ends.
    pub data_offset: usize,
}

/// `Ceil(Log2(n))`, the bits of `slice_segment_address`.
fn ceil_log2(n: u32) -> u32 {
    let mut bits = 0;
    while (1u64 << bits) < u64::from(n) {
        bits += 1;
    }
    bits
}

/// Whether `nal_unit_type` is an IDR picture.
pub const fn is_idr(unit_type: u8) -> bool {
    unit_type == IDR_W_RADL || unit_type == IDR_N_LP
}

/// Parses the header of an intra IDR slice segment. `rbsp` is the NAL unit
/// after [`crate::hevc::bits::unescape`], header included. `sps` and `pps` are the
/// ones the slice names (the caller looks them up by `pps_id` and the PPS's
/// `sps_id`).
pub fn parse_slice_header(rbsp: &[u8], unit_type: u8, sps: &Sps, pps: &Pps) -> Result<SliceHeader> {
    if !is_idr(unit_type) {
        return Err(Error::Unsupported("a slice of a picture that is not IDR"));
    }
    if pps.tiles_enabled || pps.entropy_coding_sync_enabled {
        return Err(Error::Unsupported("tiles or wavefront entry points"));
    }
    if sps.scaling_list_enabled {
        return Err(Error::Unsupported("scaling lists"));
    }
    let mut r = BitReader::new(rbsp);
    r.skip(16)?; // the NAL unit header
    let first = r.flag()?;
    let no_output_of_prior_pics = r.flag()?; // IRAP: nal_unit_type 16 to 23
    let pps_id = r.ue_max(63, "slice_pic_parameter_set_id")? as u8;
    let mut h = SliceHeader {
        first_slice_segment_in_pic: first,
        no_output_of_prior_pics,
        pps_id,
        dependent: false,
        segment_address: 0,
        slice_type: 0,
        pic_output: true,
        colour_plane_id: 0,
        sao_luma: false,
        sao_chroma: false,
        qp_delta: 0,
        cb_qp_offset: 0,
        cr_qp_offset: 0,
        deblocking_disabled: pps.deblocking_filter_disabled,
        beta_offset_div2: i32::from(pps.beta_offset_div2),
        tc_offset_div2: i32::from(pps.tc_offset_div2),
        loop_filter_across_slices: pps.loop_filter_across_slices,
        num_entry_point_offsets: 0,
        data_offset: 0,
    };
    if !first {
        if pps.dependent_slice_segments_enabled {
            h.dependent = r.flag()?;
        }
        let ctbs = sps
            .ctbs()
            .and_then(|(w, hgt)| w.checked_mul(hgt))
            .ok_or(Error::Invalid("the SPS's picture size in CTBs"))?;
        h.segment_address = r.bits(ceil_log2(ctbs))?;
        if h.segment_address >= ctbs {
            return Err(Error::Invalid("slice_segment_address"));
        }
    }
    if h.dependent {
        // A dependent segment carries only the entry points and the extension.
        return Err(Error::Unsupported("dependent slice segments"));
    }
    r.skip(pps.num_extra_slice_header_bits as usize)?;
    h.slice_type = r.ue()?;
    if h.slice_type != SLICE_I {
        return Err(Error::Unsupported("a slice that is not intra"));
    }
    if pps.output_flag_present {
        h.pic_output = r.flag()?;
    }
    if sps.separate_colour_plane {
        h.colour_plane_id = r.bits(2)? as u8;
    }
    if sps.sao_enabled {
        h.sao_luma = r.flag()?;
        if sps.chroma_array_type() != 0 {
            h.sao_chroma = r.flag()?;
        }
    }
    // SliceQpY = 26 + init_qp_minus26 + slice_qp_delta stays in -QpBdOffsetY..=51.
    h.qp_delta = r.se_range(-125, 125, "slice_qp_delta")?;
    if pps.slice_chroma_qp_offsets_present {
        h.cb_qp_offset = r.se_range(-12, 12, "slice_cb_qp_offset")?;
        h.cr_qp_offset = r.se_range(-12, 12, "slice_cr_qp_offset")?;
    }
    let mut override_deblocking = false;
    if pps.deblocking_filter_override_enabled {
        override_deblocking = r.flag()?;
    }
    if override_deblocking {
        h.deblocking_disabled = r.flag()?;
        if !h.deblocking_disabled {
            h.beta_offset_div2 = r.se_range(-6, 6, "slice_beta_offset_div2")?;
            h.tc_offset_div2 = r.se_range(-6, 6, "slice_tc_offset_div2")?;
        } else {
            h.beta_offset_div2 = 0;
            h.tc_offset_div2 = 0;
        }
    }
    if pps.loop_filter_across_slices && (h.sao_luma || h.sao_chroma || !h.deblocking_disabled) {
        h.loop_filter_across_slices = r.flag()?;
    }
    if pps.slice_header_extension_present {
        let length = r.ue_max(256, "slice_segment_header_extension_length")?;
        r.skip(8 * length as usize)?;
    }
    r.byte_alignment()?;
    h.data_offset = r.byte_offset();
    if h.data_offset > rbsp.len() {
        return Err(Error::Truncated);
    }
    Ok(h)
}
