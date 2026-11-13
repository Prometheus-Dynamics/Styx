//! The header of a slice (7.3.3) of a frame, with its reference picture list modifications
//! (7.3.3.1), prediction weight table (7.3.3.2) and decoded reference picture marking
//! (7.3.3.3). The slice data is not read: [`SliceHeader::data_bit_offset`] is where it starts.
//!
//! Field pictures are refused as unsupported, and so is a slice group change cycle (FMO is
//! refused by the PPS). Everything else a frame's slice header carries is read.

use crate::bitstream::{BitReader, Error, Result};
use crate::h264::ps::{Pps, Sps};

/// The most reference picture list modifications one list takes (one per active entry).
pub const MAX_RPL: usize = 32;
/// The most memory management control operations one picture takes.
pub const MAX_MMCO: usize = 66;

/// `slice_type` (modulo 5): what the slice's macroblocks are predicted from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceKind {
    P,
    B,
    I,
    SP,
    SI,
}

impl SliceKind {
    /// Whether the slice takes a reference picture list 0 (P, SP and B).
    pub const fn has_list0(self) -> bool {
        matches!(self, Self::P | Self::SP | Self::B)
    }

    /// Whether the slice takes a reference picture list 1 (B).
    pub const fn has_list1(self) -> bool {
        matches!(self, Self::B)
    }
}

/// One `ref_pic_list_modification` operation: `idc` 0 or 1 gives `abs_diff_pic_num_minus1`,
/// `idc` 2 gives `long_term_pic_num`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RplMod {
    pub idc: u8,
    pub value: u32,
}

/// One `memory_management_control_operation` (7.3.3.3), with its values by name:
/// `a` is `difference_of_pic_nums_minus1` (1 and 3), `long_term_pic_num` (2), and
/// `b` is `long_term_frame_idx` (3 and 6) or `max_long_term_frame_idx_plus1` (4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mmco {
    pub op: u8,
    pub a: u32,
    pub b: u32,
}

/// The explicit weights of one reference index: luma `(weight, offset)`, and chroma Cb and Cr.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WeightEntry {
    pub luma: Option<(i32, i32)>,
    pub chroma: Option<[(i32, i32); 2]>,
}

/// The prediction weight table (7.3.3.2): the denominators and one entry per active index of
/// each list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredWeights {
    pub luma_log2_denom: u8,
    pub chroma_log2_denom: u8,
    /// Entries of list 0 and list 1; only the first `num_ref_idx_active` of each are read.
    pub entries: [[WeightEntry; MAX_RPL]; 2],
}

/// A slice segment's header, of a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SliceHeader {
    /// `nal_unit_type` (1 or 5).
    pub nal_unit_type: u8,
    /// `nal_ref_idc`: nonzero for a reference picture.
    pub nal_ref_idc: u8,
    /// `first_mb_in_slice`.
    pub first_mb_in_slice: u32,
    /// `slice_type` as the bitstream gives it (0 to 9).
    pub slice_type: u32,
    /// The slice's kind, `slice_type` modulo 5.
    pub kind: SliceKind,
    /// `pic_parameter_set_id`.
    pub pps_id: u8,
    /// `colour_plane_id` (0 without separate colour planes).
    pub colour_plane_id: u8,
    /// `frame_num`.
    pub frame_num: u32,
    /// `idr_pic_id` (IDR pictures).
    pub idr_pic_id: u32,
    /// `pic_order_cnt_lsb` (type 0).
    pub poc_lsb: u32,
    /// `delta_pic_order_cnt_bottom` (type 0, with bottom_field_pic_order_in_frame_present).
    pub delta_poc_bottom: i32,
    /// `delta_pic_order_cnt[0]` and `[1]` (type 1).
    pub delta_poc: [i32; 2],
    /// `redundant_pic_cnt` (0 for a primary picture).
    pub redundant_pic_cnt: u32,
    /// `direct_spatial_mv_pred_flag` (B slices).
    pub direct_spatial_mv_pred: bool,
    /// `num_ref_idx_lX_active_minus1 + 1` for lists 0 and 1 (0 for a list the slice lacks).
    pub num_ref_idx_active: [u8; 2],
    /// Whether `ref_pic_list_modification_flag_lX` is set, for lists 0 and 1.
    pub rpl_modified: [bool; 2],
    /// The modifications of lists 0 and 1; the first `rpl_count` of each are read.
    pub rpl: [[RplMod; MAX_RPL]; 2],
    pub rpl_count: [u8; 2],
    /// `no_output_of_prior_pics_flag` (IDR pictures).
    pub no_output_of_prior_pics: bool,
    /// `long_term_reference_flag` (IDR pictures).
    pub long_term_reference: bool,
    /// `adaptive_ref_pic_marking_mode_flag` (non-IDR reference pictures).
    pub adaptive_marking: bool,
    /// The memory management operations; the first `mmco_count` are read.
    pub mmco: [Mmco; MAX_MMCO],
    pub mmco_count: u8,
    /// The prediction weight table, when the PPS and slice kind call for one.
    pub weights: Option<Box<PredWeights>>,
    /// `cabac_init_idc` (CABAC slices other than I and SI).
    pub cabac_init_idc: u8,
    /// `slice_qp_delta`.
    pub qp_delta: i32,
    /// `sp_for_switch_flag` (SP slices).
    pub sp_for_switch: bool,
    /// `slice_qs_delta` (SP and SI slices).
    pub qs_delta: i32,
    /// `disable_deblocking_filter_idc` (0 when the PPS has no deblocking control).
    pub disable_deblocking_filter_idc: u8,
    /// `slice_alpha_c0_offset_div2`.
    pub alpha_c0_offset_div2: i32,
    /// `slice_beta_offset_div2`.
    pub beta_offset_div2: i32,
    /// The bit offset of the slice data in the RBSP (header included). The data's first
    /// syntax element, `cabac_alignment_one_bit` for CABAC, is at this bit.
    pub data_bit_offset: usize,
    /// The size in bits of the picture order count syntax (`pic_order_cnt_lsb` through the
    /// delta counts), 0 when the stream's type has none.
    pub poc_bits: u32,
    /// The size in bits of `dec_ref_pic_marking()`, 0 for a non-reference picture.
    pub marking_bits: u32,
}

impl SliceHeader {
    /// The modifications of list `list` (0 or 1).
    pub fn modifications(&self, list: usize) -> &[RplMod] {
        &self.rpl[list][..usize::from(self.rpl_count[list])]
    }

    /// The memory management operations.
    pub fn mmcos(&self) -> &[Mmco] {
        &self.mmco[..usize::from(self.mmco_count)]
    }

    /// Whether this is an IDR picture.
    pub const fn is_idr(&self) -> bool {
        self.nal_unit_type == 5
    }
}

/// Reads `weighted` entries of one list: each is a flag and, when set, its weights.
fn weights_for(r: &mut BitReader<'_>, n: usize, chroma: bool) -> Result<[WeightEntry; MAX_RPL]> {
    let mut out = [WeightEntry::default(); MAX_RPL];
    for entry in out.iter_mut().take(n) {
        if r.flag()? {
            entry.luma = Some((
                r.se_range(-128, 127, "luma_weight")?,
                r.se_range(-128, 127, "luma_offset")?,
            ));
        }
        if chroma && r.flag()? {
            let mut c = [(0, 0); 2];
            for pair in &mut c {
                *pair = (
                    r.se_range(-128, 127, "chroma_weight")?,
                    r.se_range(-128, 127, "chroma_offset")?,
                );
            }
            entry.chroma = Some(c);
        }
    }
    Ok(out)
}

/// One list's `ref_pic_list_modification()` operations, up to the terminating idc 3.
fn modifications(r: &mut BitReader<'_>, out: &mut [RplMod; MAX_RPL]) -> Result<u8> {
    let mut n = 0usize;
    loop {
        let idc = r.ue_max(5, "modification_of_pic_nums_idc")? as u8;
        match idc {
            3 => return Ok(n as u8),
            0..=2 => {
                if n == MAX_RPL {
                    return Err(Error::Invalid(
                        "more reference list modifications than entries",
                    ));
                }
                let value = r.ue_max(65_535, "abs_diff_pic_num_minus1 or long_term_pic_num")?;
                out[n] = RplMod { idc, value };
                n += 1;
            }
            _ => return Err(Error::Unsupported("MVC reference list modification")),
        }
    }
}

/// Parses the header of a frame's slice. `rbsp` is the NAL unit after
/// [`crate::bitstream::unescape`], header included. `sps` and `pps` are the ones the slice names:
/// the caller reads the PPS id with [`crate::h264::ps::peek_pps_id`] and looks them up.
pub fn parse_slice_header(
    rbsp: &[u8],
    nal_unit_type: u8,
    nal_ref_idc: u8,
    sps: &Sps,
    pps: &Pps,
) -> Result<SliceHeader> {
    if nal_unit_type != 1 && nal_unit_type != 5 {
        return Err(Error::Unsupported(
            "a slice NAL unit that is not a frame slice",
        ));
    }
    if nal_unit_type == 5 && nal_ref_idc == 0 {
        return Err(Error::Invalid("an IDR picture with nal_ref_idc 0"));
    }
    if pps.id != crate::h264::ps::peek_pps_id(rbsp)? {
        return Err(Error::Invalid(
            "the PPS does not match the slice's pic_parameter_set_id",
        ));
    }
    if pps.sps_id != sps.id {
        return Err(Error::Invalid("the SPS does not match the PPS"));
    }
    let idr = nal_unit_type == 5;
    let mut r = BitReader::new(rbsp);
    r.skip(8)?; // the NAL unit header
    let first_mb_in_slice = r.ue()?;
    let slice_type = r.ue_max(9, "slice_type")?;
    let kind = match slice_type % 5 {
        0 => SliceKind::P,
        1 => SliceKind::B,
        2 => SliceKind::I,
        3 => SliceKind::SP,
        _ => SliceKind::SI,
    };
    let _pps_id = r.ue()?;
    let colour_plane_id = if sps.separate_colour_plane {
        r.bits(2)? as u8
    } else {
        0
    };
    let frame_num = r.bits(u32::from(sps.log2_max_frame_num))?;
    if !sps.frame_mbs_only && r.flag()? {
        return Err(Error::Unsupported("field pictures"));
    }
    let idr_pic_id = if idr {
        r.ue_max(65_535, "idr_pic_id")?
    } else {
        0
    };
    let mut h = SliceHeader {
        nal_unit_type,
        nal_ref_idc,
        first_mb_in_slice,
        slice_type,
        kind,
        pps_id: pps.id,
        colour_plane_id,
        frame_num,
        idr_pic_id,
        poc_lsb: 0,
        delta_poc_bottom: 0,
        delta_poc: [0; 2],
        redundant_pic_cnt: 0,
        direct_spatial_mv_pred: false,
        num_ref_idx_active: [0; 2],
        rpl_modified: [false; 2],
        rpl: [[RplMod::default(); MAX_RPL]; 2],
        rpl_count: [0; 2],
        no_output_of_prior_pics: false,
        long_term_reference: false,
        adaptive_marking: false,
        mmco: [Mmco::default(); MAX_MMCO],
        mmco_count: 0,
        weights: None,
        cabac_init_idc: 0,
        qp_delta: 0,
        sp_for_switch: false,
        qs_delta: 0,
        disable_deblocking_filter_idc: 0,
        alpha_c0_offset_div2: 0,
        beta_offset_div2: 0,
        data_bit_offset: 0,
        poc_bits: 0,
        marking_bits: 0,
    };
    let poc_start = r.bit_pos();
    match sps.poc_type {
        0 => {
            h.poc_lsb = r.bits(u32::from(sps.log2_max_poc_lsb))?;
            if pps.bottom_field_pic_order_present {
                h.delta_poc_bottom = r.se()?;
            }
        }
        1 if !sps.delta_pic_order_always_zero => {
            h.delta_poc[0] = r.se()?;
            if pps.bottom_field_pic_order_present {
                h.delta_poc[1] = r.se()?;
            }
        }
        _ => {}
    }
    h.poc_bits = (r.bit_pos() - poc_start) as u32;
    if pps.redundant_pic_cnt_present {
        h.redundant_pic_cnt = r.ue_max(127, "redundant_pic_cnt")?;
    }
    if kind == SliceKind::B {
        h.direct_spatial_mv_pred = r.flag()?;
    }
    if kind.has_list0() {
        h.num_ref_idx_active = pps.num_ref_idx_default;
        if kind == SliceKind::I {
            h.num_ref_idx_active = [0; 2];
        }
        if r.flag()? {
            h.num_ref_idx_active[0] = r.ue_max(31, "num_ref_idx_l0_active_minus1")? as u8 + 1;
            if kind.has_list1() {
                h.num_ref_idx_active[1] = r.ue_max(31, "num_ref_idx_l1_active_minus1")? as u8 + 1;
            }
        }
        if !kind.has_list1() {
            h.num_ref_idx_active[1] = 0;
        }
        for list in 0..2 {
            if (list == 0 && kind.has_list0()) || (list == 1 && kind.has_list1()) {
                h.rpl_modified[list] = r.flag()?;
                if h.rpl_modified[list] {
                    h.rpl_count[list] = modifications(&mut r, &mut h.rpl[list])?;
                }
            }
        }
    }
    let weighted = (pps.weighted_pred && matches!(kind, SliceKind::P | SliceKind::SP))
        || (pps.weighted_bipred_idc == 1 && kind == SliceKind::B);
    if weighted {
        let luma_log2_denom = r.ue_max(7, "luma_log2_weight_denom")? as u8;
        let chroma = sps.chroma_array_type() != 0;
        let chroma_log2_denom = if chroma {
            r.ue_max(7, "chroma_log2_weight_denom")? as u8
        } else {
            0
        };
        let mut entries = [[WeightEntry::default(); MAX_RPL]; 2];
        entries[0] = weights_for(&mut r, usize::from(h.num_ref_idx_active[0]), chroma)?;
        if kind.has_list1() {
            entries[1] = weights_for(&mut r, usize::from(h.num_ref_idx_active[1]), chroma)?;
        }
        h.weights = Some(Box::new(PredWeights {
            luma_log2_denom,
            chroma_log2_denom,
            entries,
        }));
    }
    let marking_start = r.bit_pos();
    if nal_ref_idc != 0 {
        if idr {
            h.no_output_of_prior_pics = r.flag()?;
            h.long_term_reference = r.flag()?;
        } else {
            h.adaptive_marking = r.flag()?;
            if h.adaptive_marking {
                let mut n = 0usize;
                loop {
                    let op = r.ue_max(6, "memory_management_control_operation")? as u8;
                    if op == 0 {
                        break;
                    }
                    if n == MAX_MMCO {
                        return Err(Error::Invalid(
                            "more memory management operations than allowed",
                        ));
                    }
                    let mut m = Mmco { op, a: 0, b: 0 };
                    match op {
                        1 => m.a = r.ue()?,
                        2 => m.a = r.ue()?,
                        3 => {
                            m.a = r.ue()?;
                            m.b = r.ue()?;
                        }
                        4 => m.b = r.ue_max(16, "max_long_term_frame_idx_plus1")?,
                        6 => m.b = r.ue()?,
                        _ => {}
                    }
                    h.mmco[n] = m;
                    n += 1;
                }
                h.mmco_count = n as u8;
            }
        }
        h.marking_bits = (r.bit_pos() - marking_start) as u32;
    }
    if pps.cabac && !matches!(kind, SliceKind::I | SliceKind::SI) {
        h.cabac_init_idc = r.ue_max(2, "cabac_init_idc")? as u8;
    }
    h.qp_delta = r.se_range(-(26 + 48), 25, "slice_qp_delta")?;
    if matches!(kind, SliceKind::SP | SliceKind::SI) {
        if kind == SliceKind::SP {
            h.sp_for_switch = r.flag()?;
        }
        h.qs_delta = r.se_range(-26, 25, "slice_qs_delta")?;
    }
    if pps.deblocking_filter_control_present {
        h.disable_deblocking_filter_idc = r.ue_max(2, "disable_deblocking_filter_idc")? as u8;
        if h.disable_deblocking_filter_idc != 1 {
            h.alpha_c0_offset_div2 = r.se_range(-6, 6, "slice_alpha_c0_offset_div2")?;
            h.beta_offset_div2 = r.se_range(-6, 6, "slice_beta_offset_div2")?;
        }
    }
    h.data_bit_offset = r.bit_pos();
    if h.data_bit_offset > rbsp.len() * 8 {
        return Err(Error::Truncated);
    }
    Ok(h)
}
