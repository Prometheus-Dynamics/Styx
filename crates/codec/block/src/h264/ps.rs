//! The sequence and picture parameter sets (7.3.2.1 and 7.3.2.2), with the scaling lists kept
//! as parsed. A list's values are in zigzag order, as the bitstream gives them; a list with
//! `use_default` set takes the default matrix, which the caller substitutes.

use crate::bitstream::{BitReader, Error, Result};

/// The number of scaling lists a SPS or PPS can carry: six 4x4 lists, then the 8x8 lists.
pub const SCALING_LISTS: usize = 12;

/// `profile_idc` values whose SPS carries the chroma format and the scaling matrix syntax.
const HIGH_PROFILES: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// The scaling lists of a SPS or PPS. Lists 0 to 5 are 4x4 (16 values used), 6 to 11 are 8x8
/// (64 values used).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScalingLists {
    /// `seq_scaling_list_present_flag` or `pic_scaling_list_present_flag` for each list.
    pub present: [bool; SCALING_LISTS],
    /// `useDefaultScalingMatrixFlag`: the list is the default matrix, not `values`.
    pub use_default: [bool; SCALING_LISTS],
    /// The list's values in zigzag order (16 or 64 used, the rest zero).
    pub values: [[u8; 64]; SCALING_LISTS],
}

impl Default for ScalingLists {
    fn default() -> Self {
        Self {
            present: [false; SCALING_LISTS],
            use_default: [false; SCALING_LISTS],
            values: [[0; 64]; SCALING_LISTS],
        }
    }
}

/// A sequence parameter set (7.3.2.1.1), with what a decoder reads from it. The VUI is not
/// read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    /// `constraint_set0_flag` to `constraint_set5_flag` in bits 7 to 2, as the byte has them.
    pub constraint_flags: u8,
    pub level_idc: u8,
    /// `seq_parameter_set_id` (0 to 31).
    pub id: u8,
    /// `chroma_format_idc` (0 grey, 1 4:2:0, 2 4:2:2, 3 4:4:4); 1 for the profiles without it.
    pub chroma_format_idc: u8,
    /// `separate_colour_plane_flag`.
    pub separate_colour_plane: bool,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    /// `qpprime_y_zero_transform_bypass_flag`.
    pub transform_bypass: bool,
    /// The scaling lists, when `seq_scaling_matrix_present_flag` is set.
    pub scaling: Option<Box<ScalingLists>>,
    /// `log2_max_frame_num_minus4 + 4` (4 to 16).
    pub log2_max_frame_num: u8,
    /// `pic_order_cnt_type` (0 to 2).
    pub poc_type: u8,
    /// `log2_max_pic_order_cnt_lsb_minus4 + 4`, for type 0 (4 to 16).
    pub log2_max_poc_lsb: u8,
    /// `delta_pic_order_always_zero_flag`, for type 1.
    pub delta_pic_order_always_zero: bool,
    /// `offset_for_non_ref_pic`, for type 1.
    pub offset_for_non_ref_pic: i32,
    /// `offset_for_top_to_bottom_field`, for type 1.
    pub offset_for_top_to_bottom_field: i32,
    /// `offset_for_ref_frame[i]`, for type 1: one per `num_ref_frames_in_pic_order_cnt_cycle`.
    pub offset_for_ref_frame: Vec<i32>,
    /// `max_num_ref_frames` (0 to 16).
    pub max_num_ref_frames: u8,
    /// `gaps_in_frame_num_value_allowed_flag`.
    pub gaps_allowed: bool,
    /// `pic_width_in_mbs_minus1 + 1`.
    pub width_mbs: u32,
    /// `pic_height_in_map_units_minus1 + 1`.
    pub height_map_units: u32,
    /// `frame_mbs_only_flag`.
    pub frame_mbs_only: bool,
    /// `mb_adaptive_frame_field_flag` (MBAFF frames).
    pub mb_adaptive_frame_field: bool,
    /// `direct_8x8_inference_flag`.
    pub direct_8x8_inference: bool,
    /// The frame cropping offsets (left, right, top, bottom), in crop units, when cropped.
    pub crop: Option<[u32; 4]>,
    /// `vui_parameters_present_flag` (the VUI itself is not read).
    pub vui_present: bool,
}

impl Sps {
    /// `ChromaArrayType`: 0 when the colour planes are separate or the format is grey.
    pub const fn chroma_array_type(&self) -> u8 {
        if self.separate_colour_plane {
            0
        } else {
            self.chroma_format_idc
        }
    }

    /// `MaxFrameNum`.
    pub const fn max_frame_num(&self) -> u32 {
        1 << self.log2_max_frame_num
    }

    /// `FrameHeightInMbs` (a frame's height in macroblocks).
    pub const fn frame_height_mbs(&self) -> u32 {
        (if self.frame_mbs_only { 1 } else { 2 }) * self.height_map_units
    }

    /// The luma size of a frame in samples, after cropping, as (width, height).
    pub fn frame_size(&self) -> (u32, u32) {
        let width = self.width_mbs * 16;
        let height = self.frame_height_mbs() * 16;
        let Some([left, right, top, bottom]) = self.crop else {
            return (width, height);
        };
        let (sub_w, sub_h) = match self.chroma_array_type() {
            0 => (1, 1),
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let unit_y = sub_h * if self.frame_mbs_only { 1 } else { 2 };
        let w = width.saturating_sub(left.saturating_add(right).saturating_mul(sub_w));
        let h = height.saturating_sub(top.saturating_add(bottom).saturating_mul(unit_y));
        (w, h)
    }
}

/// A picture parameter set (7.3.2.2), with what a decoder reads from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pps {
    /// `pic_parameter_set_id` (0 to 255).
    pub id: u8,
    /// `seq_parameter_set_id` (0 to 31).
    pub sps_id: u8,
    /// `entropy_coding_mode_flag` (1 for CABAC).
    pub cabac: bool,
    /// `bottom_field_pic_order_in_frame_present_flag`.
    pub bottom_field_pic_order_present: bool,
    /// `num_ref_idx_l0_default_active_minus1 + 1` and the l1 count.
    pub num_ref_idx_default: [u8; 2],
    /// `weighted_pred_flag`.
    pub weighted_pred: bool,
    /// `weighted_bipred_idc` (0 to 2).
    pub weighted_bipred_idc: u8,
    /// `pic_init_qp_minus26 + 26`.
    pub pic_init_qp: i32,
    /// `pic_init_qs_minus26 + 26`.
    pub pic_init_qs: i32,
    /// `chroma_qp_index_offset`.
    pub chroma_qp_index_offset: i32,
    /// `second_chroma_qp_index_offset` (equal to `chroma_qp_index_offset` without the extension).
    pub second_chroma_qp_index_offset: i32,
    /// `deblocking_filter_control_present_flag`.
    pub deblocking_filter_control_present: bool,
    /// `constrained_intra_pred_flag`.
    pub constrained_intra_pred: bool,
    /// `redundant_pic_cnt_present_flag`.
    pub redundant_pic_cnt_present: bool,
    /// `transform_8x8_mode_flag`.
    pub transform_8x8_mode: bool,
    /// The scaling lists, when `pic_scaling_matrix_present_flag` is set.
    pub scaling: Option<Box<ScalingLists>>,
}

fn invalid(what: &'static str) -> Error {
    Error::Invalid(what)
}

/// The header byte of a NAL unit: `forbidden_zero_bit` must be 0, and the type must be `want`.
fn check_header(rbsp: &[u8], want: u8) -> Result<()> {
    let first = *rbsp.first().ok_or(Error::Truncated)?;
    if first & 0x80 != 0 {
        return Err(invalid("forbidden_zero_bit"));
    }
    if first & 0x1f != want {
        return Err(invalid("the NAL unit is not of the expected type"));
    }
    Ok(())
}

/// `scaling_list()` (7.3.2.1.1.1): `size` values, delta coded, into `out`. Returns
/// `useDefaultScalingMatrixFlag`.
fn scaling_list(r: &mut BitReader<'_>, out: &mut [u8]) -> Result<bool> {
    let mut last = 8i32;
    let mut next = 8i32;
    let mut use_default = false;
    for (j, slot) in out.iter_mut().enumerate() {
        if next != 0 {
            let delta = r.se_range(-128, 127, "delta_scale")?;
            next = (last + delta + 256).rem_euclid(256);
            use_default = j == 0 && next == 0;
        }
        let value = if next == 0 { last } else { next };
        *slot = value as u8;
        last = value;
    }
    Ok(use_default)
}

/// The scaling lists of a set with `count` lists present in the flags (`count` 8 or 12 for a
/// SPS, 6 + 2 or 6 + 6 for a PPS). List `i` has 16 values below 6 and 64 from 6.
fn scaling_lists(r: &mut BitReader<'_>, count: usize) -> Result<ScalingLists> {
    let mut s = ScalingLists::default();
    for i in 0..count {
        s.present[i] = r.flag()?;
        if s.present[i] {
            let size = if i < 6 { 16 } else { 64 };
            s.use_default[i] = scaling_list(r, &mut s.values[i][..size])?;
        }
    }
    Ok(s)
}

/// Parses a sequence parameter set from its RBSP (the NAL unit with its one-byte header).
pub fn parse_sps(rbsp: &[u8]) -> Result<Sps> {
    check_header(rbsp, 7)?;
    let mut r = BitReader::new(rbsp);
    r.skip(8)?;
    let profile_idc = r.bits(8)? as u8;
    let constraint_flags = r.bits(8)? as u8;
    let level_idc = r.bits(8)? as u8;
    let id = r.ue_max(31, "seq_parameter_set_id")? as u8;
    let mut sps = Sps {
        profile_idc,
        constraint_flags,
        level_idc,
        id,
        chroma_format_idc: 1,
        separate_colour_plane: false,
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
        transform_bypass: false,
        scaling: None,
        log2_max_frame_num: 4,
        poc_type: 0,
        log2_max_poc_lsb: 4,
        delta_pic_order_always_zero: false,
        offset_for_non_ref_pic: 0,
        offset_for_top_to_bottom_field: 0,
        offset_for_ref_frame: Vec::new(),
        max_num_ref_frames: 0,
        gaps_allowed: false,
        width_mbs: 0,
        height_map_units: 0,
        frame_mbs_only: true,
        mb_adaptive_frame_field: false,
        direct_8x8_inference: false,
        crop: None,
        vui_present: false,
    };
    if HIGH_PROFILES.contains(&profile_idc) {
        sps.chroma_format_idc = r.ue_max(3, "chroma_format_idc")? as u8;
        if sps.chroma_format_idc == 3 {
            sps.separate_colour_plane = r.flag()?;
        }
        sps.bit_depth_luma_minus8 = r.ue_max(6, "bit_depth_luma_minus8")? as u8;
        sps.bit_depth_chroma_minus8 = r.ue_max(6, "bit_depth_chroma_minus8")? as u8;
        sps.transform_bypass = r.flag()?;
        if r.flag()? {
            let count = if sps.chroma_format_idc != 3 { 8 } else { 12 };
            sps.scaling = Some(Box::new(scaling_lists(&mut r, count)?));
        }
    }
    sps.log2_max_frame_num = r.ue_max(12, "log2_max_frame_num_minus4")? as u8 + 4;
    sps.poc_type = r.ue_max(2, "pic_order_cnt_type")? as u8;
    match sps.poc_type {
        0 => sps.log2_max_poc_lsb = r.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? as u8 + 4,
        1 => {
            sps.delta_pic_order_always_zero = r.flag()?;
            sps.offset_for_non_ref_pic = r.se()?;
            sps.offset_for_top_to_bottom_field = r.se()?;
            let cycle = r.ue_max(255, "num_ref_frames_in_pic_order_cnt_cycle")?;
            for _ in 0..cycle {
                sps.offset_for_ref_frame.push(r.se()?);
            }
        }
        _ => {}
    }
    sps.max_num_ref_frames = r.ue_max(16, "max_num_ref_frames")? as u8;
    sps.gaps_allowed = r.flag()?;
    sps.width_mbs = r.ue_max(8191, "pic_width_in_mbs_minus1")? + 1;
    sps.height_map_units = r.ue_max(8191, "pic_height_in_map_units_minus1")? + 1;
    sps.frame_mbs_only = r.flag()?;
    if !sps.frame_mbs_only {
        sps.mb_adaptive_frame_field = r.flag()?;
    }
    sps.direct_8x8_inference = r.flag()?;
    if r.flag()? {
        let mut crop = [0u32; 4];
        for c in &mut crop {
            *c = r.ue()?;
        }
        sps.crop = Some(crop);
    }
    sps.vui_present = r.flag()?;
    Ok(sps)
}

/// The `pic_parameter_set_id` of a slice's RBSP, read before the slice is parsed, since the
/// PPS it names is what the rest of the header is parsed with.
pub fn peek_pps_id(rbsp: &[u8]) -> Result<u8> {
    let mut r = BitReader::new(rbsp);
    r.skip(8)?;
    r.ue()?;
    r.ue()?;
    Ok(r.ue_max(255, "pic_parameter_set_id")? as u8)
}

/// Parses a picture parameter set from its RBSP. `sps_chroma_format` gives the
/// `chroma_format_idc` of a SPS by its id; it is asked only when the PPS carries scaling lists,
/// whose count depends on it.
pub fn parse_pps(rbsp: &[u8], sps_chroma_format: impl FnOnce(u8) -> Option<u8>) -> Result<Pps> {
    check_header(rbsp, 8)?;
    let mut r = BitReader::new(rbsp);
    r.skip(8)?;
    let id = r.ue_max(255, "pic_parameter_set_id")? as u8;
    let sps_id = r.ue_max(31, "seq_parameter_set_id")? as u8;
    let cabac = r.flag()?;
    let bottom_field_pic_order_present = r.flag()?;
    if r.ue()? != 0 {
        return Err(Error::Unsupported("slice groups (FMO)"));
    }
    let num_ref_idx_default = [
        r.ue_max(31, "num_ref_idx_l0_default_active_minus1")? as u8 + 1,
        r.ue_max(31, "num_ref_idx_l1_default_active_minus1")? as u8 + 1,
    ];
    let weighted_pred = r.flag()?;
    let weighted_bipred_idc = r.bits(2)? as u8;
    if weighted_bipred_idc > 2 {
        return Err(invalid("weighted_bipred_idc"));
    }
    let pic_init_qp = 26 + r.se_range(-(26 + 6 * 8), 25, "pic_init_qp_minus26")?;
    let pic_init_qs = 26 + r.se_range(-26, 25, "pic_init_qs_minus26")?;
    let chroma_qp_index_offset = r.se_range(-12, 12, "chroma_qp_index_offset")?;
    let deblocking_filter_control_present = r.flag()?;
    let constrained_intra_pred = r.flag()?;
    let redundant_pic_cnt_present = r.flag()?;
    let mut pps = Pps {
        id,
        sps_id,
        cabac,
        bottom_field_pic_order_present,
        num_ref_idx_default,
        weighted_pred,
        weighted_bipred_idc,
        pic_init_qp,
        pic_init_qs,
        chroma_qp_index_offset,
        second_chroma_qp_index_offset: chroma_qp_index_offset,
        deblocking_filter_control_present,
        constrained_intra_pred,
        redundant_pic_cnt_present,
        transform_8x8_mode: false,
        scaling: None,
    };
    if r.more_rbsp_data() {
        pps.transform_8x8_mode = r.flag()?;
        if r.flag()? {
            let chroma = sps_chroma_format(sps_id).ok_or(invalid("a PPS before its SPS"))?;
            let eight = if chroma != 3 { 2 } else { 6 };
            let count = 6 + if pps.transform_8x8_mode { eight } else { 0 };
            pps.scaling = Some(Box::new(scaling_lists(&mut r, count)?));
        }
        pps.second_chroma_qp_index_offset = r.se_range(-12, 12, "second_chroma_qp_index_offset")?;
    }
    Ok(pps)
}
