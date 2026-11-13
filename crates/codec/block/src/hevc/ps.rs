//! Parameter sets: the VPS (identifier only), the SPS and the PPS, as the
//! specification's syntax (ITU-T H.265, 7.3.2) gives them. The fields a
//! decoder's control structures need are kept; the rest is read and dropped.
//! Everything after the SPS's VUI and the PPS's range, multilayer and 3D
//! extensions is not read.

use crate::hevc::bits::{BitReader, Error, Result};

/// The NAL header is two bytes; parameter-set syntax starts after it.
const HEADER_BYTES: usize = 2;

/// `sps_max_sub_layers_minus1`, up to 6 sub-layers plus the base layer.
const MAX_SUB_LAYERS: u32 = 6;
/// `num_short_term_ref_pic_sets` is at most 64.
const MAX_ST_RPS: u32 = 64;
/// `num_long_term_ref_pics_sps` is at most 32.
const MAX_LT_SPS: u32 = 32;
/// `num_negative_pics` and `num_positive_pics` are each at most 16.
const MAX_RPS_PICS: usize = 16;
/// `DeltaPocS0` and `DeltaPocS1` lie in -2^15 to 2^15 - 1 (7.4.8).
const DELTA_POC_RANGE: core::ops::RangeInclusive<i32> = -(1 << 15)..=(1 << 15) - 1;

/// `a + b` for two POC deltas, refused when it leaves [`DELTA_POC_RANGE`].
fn delta_poc(a: i32, b: i32) -> Result<i32> {
    a.checked_add(b)
        .filter(|d| DELTA_POC_RANGE.contains(d))
        .ok_or(Error::Invalid("a short-term picture's POC delta"))
}
/// `num_tile_columns_minus1` at most 19 and `num_tile_rows_minus1` at most 21.
const MAX_TILE_COLUMNS: usize = 20;
/// See [`MAX_TILE_COLUMNS`].
const MAX_TILE_ROWS: usize = 22;

/// A video parameter set, reduced to its identifier and sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vps {
    /// `vps_video_parameter_set_id` (4 bits).
    pub id: u8,
    /// `vps_max_layers_minus1` (6 bits).
    pub max_layers_minus1: u8,
    /// `vps_max_sub_layers_minus1` (3 bits).
    pub max_sub_layers_minus1: u8,
    /// `vps_temporal_id_nesting_flag`.
    pub temporal_id_nesting: bool,
}

/// Parses a VPS NAL unit's RBSP (header included).
pub fn parse_vps(rbsp: &[u8]) -> Result<Vps> {
    let mut r = BitReader::new(rbsp);
    r.skip(HEADER_BYTES * 8)?;
    let id = r.bits(4)? as u8;
    r.skip(2)?; // vps_base_layer_internal_flag, vps_base_layer_available_flag
    let max_layers_minus1 = r.bits(6)? as u8;
    let max_sub_layers_minus1 = r.bits(3)? as u8;
    if u32::from(max_sub_layers_minus1) >= MAX_SUB_LAYERS {
        return Err(Error::Invalid("vps_max_sub_layers_minus1"));
    }
    let temporal_id_nesting = r.flag()?;
    Ok(Vps {
        id,
        max_layers_minus1,
        max_sub_layers_minus1,
        temporal_id_nesting,
    })
}

/// `profile_tier_level(1, maxNumSubLayersMinus1)`: read past, with its sizes.
fn skip_profile_tier_level(r: &mut BitReader<'_>, max_sub_layers_minus1: u32) -> Result<()> {
    // general_profile_space to general_level_idc: 88 bits of profile, 8 of level.
    r.skip(88 + 8)?;
    let mut profile_present = [false; MAX_SUB_LAYERS as usize];
    let mut level_present = [false; MAX_SUB_LAYERS as usize];
    for i in 0..max_sub_layers_minus1 as usize {
        profile_present[i] = r.flag()?;
        level_present[i] = r.flag()?;
    }
    if max_sub_layers_minus1 > 0 {
        r.skip(2 * (8 - max_sub_layers_minus1 as usize))?; // reserved_zero_2bits
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if profile_present[i] {
            r.skip(88)?;
        }
        if level_present[i] {
            r.skip(8)?;
        }
    }
    Ok(())
}

/// One short-term reference picture set (7.3.7 and 7.4.8): the POC deltas of
/// the pictures before and after the current one. Only the previous set is
/// ever needed to predict the next, so [`parse_st_rps`] takes and gives one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StRps {
    /// `NumNegativePics`.
    pub num_negative: u8,
    /// `NumPositivePics`.
    pub num_positive: u8,
    /// `DeltaPocS0`, nearest first.
    pub delta_poc_s0: [i32; MAX_RPS_PICS],
    /// `UsedByCurrPicS0`.
    pub used_s0: [bool; MAX_RPS_PICS],
    /// `DeltaPocS1`, nearest first.
    pub delta_poc_s1: [i32; MAX_RPS_PICS],
    /// `UsedByCurrPicS1`.
    pub used_s1: [bool; MAX_RPS_PICS],
}

impl StRps {
    /// `NumDeltaPocs`.
    pub const fn num_delta_pocs(&self) -> usize {
        self.num_negative as usize + self.num_positive as usize
    }
}

/// `st_ref_pic_set(stRpsIdx)`. `index` is `stRpsIdx`, `count` is
/// `num_short_term_ref_pic_sets` and `previous` is the set at `index - 1`
/// (ignored for set 0). In an SPS the reference set is always `index - 1`,
/// which is what this takes.
pub fn parse_st_rps(r: &mut BitReader<'_>, index: u32, previous: &StRps) -> Result<StRps> {
    let predicted = index != 0 && r.flag()?;
    let mut set = StRps::default();
    if predicted {
        // An SPS's set predicts from the one before it (delta_idx_minus1 is 0).
        let sign = r.flag()?;
        let abs_minus1 = r.ue_max(0x7fff, "abs_delta_rps_minus1")?;
        // abs_delta_rps_minus1 is at most 2^15 - 1, so this fits.
        let delta_rps = if sign {
            -(abs_minus1 as i32 + 1)
        } else {
            abs_minus1 as i32 + 1
        };
        // A set built by this parser has at most 16 pictures on each side; one
        // built by hand is held to the same.
        let neg = previous.num_negative as usize;
        let pos = previous.num_positive as usize;
        if neg > MAX_RPS_PICS || pos > MAX_RPS_PICS {
            return Err(Error::Invalid(
                "a previous set with more than 16 pictures a side",
            ));
        }
        let n = neg + pos;
        // One flag pair per picture of the previous set, and one for the set's
        // own picture: up to 33.
        let mut used = [false; 2 * MAX_RPS_PICS + 1];
        let mut use_delta = [true; 2 * MAX_RPS_PICS + 1];
        for (u, d) in used.iter_mut().zip(use_delta.iter_mut()).take(n + 1) {
            *u = r.flag()?;
            if !*u {
                *d = r.flag()?;
            }
        }
        // Equations 7-61 and 7-62: the derived negative and positive lists.
        let mut i = 0;
        let push_s0 = |set: &mut StRps, dpoc: i32, u: bool, i: &mut usize| -> Result<()> {
            if *i >= MAX_RPS_PICS {
                return Err(Error::Invalid("more than 16 negative pictures in a set"));
            }
            set.delta_poc_s0[*i] = dpoc;
            set.used_s0[*i] = u;
            *i += 1;
            Ok(())
        };
        for j in (0..pos).rev() {
            let dpoc = delta_poc(previous.delta_poc_s1[j], delta_rps)?;
            if dpoc < 0 && use_delta[neg + j] {
                push_s0(&mut set, dpoc, used[neg + j], &mut i)?;
            }
        }
        if delta_rps < 0 && use_delta[n] {
            push_s0(&mut set, delta_rps, used[n], &mut i)?;
        }
        for j in 0..neg {
            let dpoc = delta_poc(previous.delta_poc_s0[j], delta_rps)?;
            if dpoc < 0 && use_delta[j] {
                push_s0(&mut set, dpoc, used[j], &mut i)?;
            }
        }
        set.num_negative = i as u8;

        let mut i = 0;
        let push_s1 = |set: &mut StRps, dpoc: i32, u: bool, i: &mut usize| -> Result<()> {
            if *i >= MAX_RPS_PICS {
                return Err(Error::Invalid("more than 16 positive pictures in a set"));
            }
            set.delta_poc_s1[*i] = dpoc;
            set.used_s1[*i] = u;
            *i += 1;
            Ok(())
        };
        for j in (0..neg).rev() {
            let dpoc = delta_poc(previous.delta_poc_s0[j], delta_rps)?;
            if dpoc > 0 && use_delta[j] {
                push_s1(&mut set, dpoc, used[j], &mut i)?;
            }
        }
        if delta_rps > 0 && use_delta[n] {
            push_s1(&mut set, delta_rps, used[n], &mut i)?;
        }
        for j in 0..pos {
            let dpoc = delta_poc(previous.delta_poc_s1[j], delta_rps)?;
            if dpoc > 0 && use_delta[neg + j] {
                push_s1(&mut set, dpoc, used[neg + j], &mut i)?;
            }
        }
        set.num_positive = i as u8;
    } else {
        let negative = r.ue_max(MAX_RPS_PICS as u32, "num_negative_pics")?;
        let positive = r.ue_max(MAX_RPS_PICS as u32, "num_positive_pics")?;
        set.num_negative = negative as u8;
        set.num_positive = positive as u8;
        let mut poc = 0i32;
        for (delta, used) in set
            .delta_poc_s0
            .iter_mut()
            .zip(set.used_s0.iter_mut())
            .take(negative as usize)
        {
            poc = delta_poc(poc, -(r.ue_max(0x7fff, "delta_poc_s0_minus1")? as i32 + 1))?;
            *delta = poc;
            *used = r.flag()?;
        }
        poc = 0;
        for (delta, used) in set
            .delta_poc_s1
            .iter_mut()
            .zip(set.used_s1.iter_mut())
            .take(positive as usize)
        {
            poc = delta_poc(poc, r.ue_max(0x7fff, "delta_poc_s1_minus1")? as i32 + 1)?;
            *delta = poc;
            *used = r.flag()?;
        }
    }
    Ok(set)
}

/// `scaling_list_data()`, read past. The scaling lists themselves are not
/// kept: a stream that enables them is refused by the slice layer.
fn skip_scaling_list_data(r: &mut BitReader<'_>) -> Result<()> {
    for size_id in 0..4u32 {
        let step = if size_id == 3 { 3 } else { 1 };
        let mut matrix_id = 0;
        while matrix_id < 6 {
            if !r.flag()? {
                r.ue()?; // scaling_list_pred_matrix_id_delta
            } else {
                let coef_num = 64.min(1u32 << (4 + (size_id << 1)));
                if size_id > 1 {
                    r.se()?; // scaling_list_dc_coef_minus8
                }
                for _ in 0..coef_num {
                    r.se()?; // scaling_list_delta_coef
                }
            }
            matrix_id += step;
        }
    }
    Ok(())
}

/// A sequence parameter set (7.3.2.2), as far as the VUI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sps {
    /// `sps_video_parameter_set_id`.
    pub vps_id: u8,
    /// `sps_max_sub_layers_minus1`.
    pub max_sub_layers_minus1: u8,
    /// `sps_seq_parameter_set_id` (0 to 15).
    pub id: u8,
    /// `chroma_format_idc` (0 monochrome, 1 4:2:0, 2 4:2:2, 3 4:4:4).
    pub chroma_format_idc: u8,
    /// `separate_colour_plane_flag`.
    pub separate_colour_plane: bool,
    /// `pic_width_in_luma_samples`.
    pub width: u32,
    /// `pic_height_in_luma_samples`.
    pub height: u32,
    /// The conformance window, in luma samples: left, right, top, bottom.
    pub conformance_window: [u32; 4],
    /// `bit_depth_luma_minus8`.
    pub bit_depth_luma_minus8: u8,
    /// `bit_depth_chroma_minus8`.
    pub bit_depth_chroma_minus8: u8,
    /// `log2_max_pic_order_cnt_lsb_minus4`.
    pub log2_max_poc_lsb_minus4: u8,
    /// `sps_max_dec_pic_buffering_minus1` of the highest sub-layer.
    pub max_dec_pic_buffering_minus1: u8,
    /// `sps_max_num_reorder_pics` of the highest sub-layer.
    pub max_num_reorder_pics: u8,
    /// `sps_max_latency_increase_plus1` of the highest sub-layer.
    pub max_latency_increase_plus1: u32,
    /// `log2_min_luma_coding_block_size_minus3`.
    pub log2_min_cb_minus3: u8,
    /// `log2_diff_max_min_luma_coding_block_size`.
    pub log2_diff_max_min_cb: u8,
    /// `log2_min_luma_transform_block_size_minus2`.
    pub log2_min_tb_minus2: u8,
    /// `log2_diff_max_min_luma_transform_block_size`.
    pub log2_diff_max_min_tb: u8,
    /// `max_transform_hierarchy_depth_inter`.
    pub max_transform_hierarchy_depth_inter: u8,
    /// `max_transform_hierarchy_depth_intra`.
    pub max_transform_hierarchy_depth_intra: u8,
    /// `scaling_list_enabled_flag`.
    pub scaling_list_enabled: bool,
    /// `sps_scaling_list_data_present_flag` (the lists are read past, not kept).
    pub scaling_list_data_present: bool,
    /// `amp_enabled_flag`.
    pub amp_enabled: bool,
    /// `sample_adaptive_offset_enabled_flag`.
    pub sao_enabled: bool,
    /// `pcm_enabled_flag`.
    pub pcm_enabled: bool,
    /// `pcm_sample_bit_depth_luma_minus1`.
    pub pcm_bit_depth_luma_minus1: u8,
    /// `pcm_sample_bit_depth_chroma_minus1`.
    pub pcm_bit_depth_chroma_minus1: u8,
    /// `log2_min_pcm_luma_coding_block_size_minus3`.
    pub log2_min_pcm_cb_minus3: u8,
    /// `log2_diff_max_min_pcm_luma_coding_block_size`.
    pub log2_diff_max_min_pcm: u8,
    /// `pcm_loop_filter_disabled_flag`.
    pub pcm_loop_filter_disabled: bool,
    /// `num_short_term_ref_pic_sets`.
    pub num_short_term_ref_pic_sets: u8,
    /// `long_term_ref_pics_present_flag`.
    pub long_term_ref_pics_present: bool,
    /// `num_long_term_ref_pics_sps`.
    pub num_long_term_ref_pics_sps: u8,
    /// `lt_ref_pic_poc_lsb_sps`.
    pub lt_ref_pic_poc_lsb_sps: [u16; MAX_LT_SPS as usize],
    /// `used_by_curr_pic_lt_sps_flag`.
    pub used_by_curr_pic_lt_sps: [bool; MAX_LT_SPS as usize],
    /// `sps_temporal_mvp_enabled_flag`.
    pub temporal_mvp_enabled: bool,
    /// `strong_intra_smoothing_enabled_flag`.
    pub strong_intra_smoothing: bool,
    /// `vui_parameters_present_flag`. The VUI is not read.
    pub vui_present: bool,
}

impl Sps {
    /// `ChromaArrayType`.
    pub const fn chroma_array_type(&self) -> u8 {
        if self.separate_colour_plane {
            0
        } else {
            self.chroma_format_idc
        }
    }

    /// `Log2MinCbSizeY`.
    pub const fn log2_min_cb(&self) -> u32 {
        self.log2_min_cb_minus3 as u32 + 3
    }

    /// `CtbLog2SizeY`.
    pub const fn log2_ctb(&self) -> u32 {
        self.log2_min_cb() + self.log2_diff_max_min_cb as u32
    }

    /// `PicWidthInCtbsY` and `PicHeightInCtbsY`, or `None` when the CTB size is
    /// outside the specification's 16 to 64 (only a hand-built SPS has one).
    pub const fn ctbs(&self) -> Option<(u32, u32)> {
        let log2 = self.log2_ctb();
        if log2 < 4 || log2 > 6 {
            return None;
        }
        let ctb = 1 << log2;
        Some((self.width.div_ceil(ctb), self.height.div_ceil(ctb)))
    }
}

/// Parses an SPS NAL unit's RBSP (header included). Reads up to the VUI flag;
/// the VUI and the extensions after it are not read.
pub fn parse_sps(rbsp: &[u8]) -> Result<Sps> {
    let mut r = BitReader::new(rbsp);
    r.skip(HEADER_BYTES * 8)?;
    let mut sps = Sps {
        vps_id: r.bits(4)? as u8,
        ..Sps::default()
    };
    let max_sub_layers_minus1 = r.bits(3)?;
    if max_sub_layers_minus1 >= MAX_SUB_LAYERS {
        return Err(Error::Invalid("sps_max_sub_layers_minus1"));
    }
    sps.max_sub_layers_minus1 = max_sub_layers_minus1 as u8;
    r.skip(1)?; // sps_temporal_id_nesting_flag
    skip_profile_tier_level(&mut r, max_sub_layers_minus1)?;
    sps.id = r.ue_max(15, "sps_seq_parameter_set_id")? as u8;
    sps.chroma_format_idc = r.ue_max(3, "chroma_format_idc")? as u8;
    if sps.chroma_format_idc == 3 {
        sps.separate_colour_plane = r.flag()?;
    }
    sps.width = r.ue()?;
    sps.height = r.ue()?;
    if sps.width == 0 || sps.height == 0 || sps.width > 16888 || sps.height > 16888 {
        return Err(Error::Invalid("picture size"));
    }
    if r.flag()? {
        // conformance_window_flag: offsets in chroma units, scaled to luma.
        let (sub_w, sub_h) = match sps.chroma_format_idc {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let (sub_w, sub_h) = if sps.separate_colour_plane {
            (1, 1)
        } else {
            (sub_w, sub_h)
        };
        let mut window = [0u32; 4];
        for (side, scale) in window.iter_mut().zip([sub_w, sub_w, sub_h, sub_h]) {
            *side = r
                .ue()?
                .checked_mul(scale)
                .ok_or(Error::Invalid("conformance window"))?;
        }
        let [left, right, top, bottom] = window;
        // The window must leave at least one sample in each direction.
        let fits = |a: u32, b: u32, size: u32| a.checked_add(b).is_some_and(|s| s < size);
        if !fits(left, right, sps.width) || !fits(top, bottom, sps.height) {
            return Err(Error::Invalid("conformance window"));
        }
        sps.conformance_window = window;
    }
    sps.bit_depth_luma_minus8 = r.ue_max(8, "bit_depth_luma_minus8")? as u8;
    sps.bit_depth_chroma_minus8 = r.ue_max(8, "bit_depth_chroma_minus8")? as u8;
    sps.log2_max_poc_lsb_minus4 = r.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? as u8;
    let ordering_all = r.flag()?;
    let first = if ordering_all {
        0
    } else {
        max_sub_layers_minus1
    };
    for _ in first..=max_sub_layers_minus1 {
        // Kept per sub-layer; the last read is the highest sub-layer's.
        sps.max_dec_pic_buffering_minus1 = r.ue_max(15, "sps_max_dec_pic_buffering_minus1")? as u8;
        sps.max_num_reorder_pics = r.ue_max(15, "sps_max_num_reorder_pics")? as u8;
        sps.max_latency_increase_plus1 = r.ue()?;
    }
    sps.log2_min_cb_minus3 = r.ue_max(3, "log2_min_luma_coding_block_size_minus3")? as u8;
    sps.log2_diff_max_min_cb = r.ue_max(3, "log2_diff_max_min_luma_coding_block_size")? as u8;
    sps.log2_min_tb_minus2 = r.ue_max(3, "log2_min_luma_transform_block_size_minus2")? as u8;
    sps.log2_diff_max_min_tb = r.ue_max(3, "log2_diff_max_min_luma_transform_block_size")? as u8;
    sps.max_transform_hierarchy_depth_inter =
        r.ue_max(4, "max_transform_hierarchy_depth_inter")? as u8;
    sps.max_transform_hierarchy_depth_intra =
        r.ue_max(4, "max_transform_hierarchy_depth_intra")? as u8;
    sps.scaling_list_enabled = r.flag()?;
    if sps.scaling_list_enabled {
        sps.scaling_list_data_present = r.flag()?;
        if sps.scaling_list_data_present {
            skip_scaling_list_data(&mut r)?;
        }
    }
    sps.amp_enabled = r.flag()?;
    sps.sao_enabled = r.flag()?;
    sps.pcm_enabled = r.flag()?;
    if sps.pcm_enabled {
        sps.pcm_bit_depth_luma_minus1 = r.bits(4)? as u8;
        sps.pcm_bit_depth_chroma_minus1 = r.bits(4)? as u8;
        sps.log2_min_pcm_cb_minus3 =
            r.ue_max(2, "log2_min_pcm_luma_coding_block_size_minus3")? as u8;
        sps.log2_diff_max_min_pcm =
            r.ue_max(2, "log2_diff_max_min_pcm_luma_coding_block_size")? as u8;
        sps.pcm_loop_filter_disabled = r.flag()?;
    }
    let num_sets = r.ue_max(MAX_ST_RPS, "num_short_term_ref_pic_sets")?;
    sps.num_short_term_ref_pic_sets = num_sets as u8;
    let mut previous = StRps::default();
    for index in 0..num_sets {
        previous = parse_st_rps(&mut r, index, &previous)?;
    }
    sps.long_term_ref_pics_present = r.flag()?;
    if sps.long_term_ref_pics_present {
        let count = r.ue_max(MAX_LT_SPS, "num_long_term_ref_pics_sps")?;
        sps.num_long_term_ref_pics_sps = count as u8;
        for (lsb, used) in sps
            .lt_ref_pic_poc_lsb_sps
            .iter_mut()
            .zip(sps.used_by_curr_pic_lt_sps.iter_mut())
            .take(count as usize)
        {
            *lsb = r.bits(u32::from(sps.log2_max_poc_lsb_minus4) + 4)? as u16;
            *used = r.flag()?;
        }
    }
    sps.temporal_mvp_enabled = r.flag()?;
    sps.strong_intra_smoothing = r.flag()?;
    sps.vui_present = r.flag()?;
    Ok(sps)
}

/// A picture parameter set (7.3.2.3). The range, multilayer and 3D extensions
/// are not read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pps {
    /// `pps_pic_parameter_set_id` (0 to 63).
    pub id: u8,
    /// `pps_seq_parameter_set_id`.
    pub sps_id: u8,
    /// `dependent_slice_segments_enabled_flag`.
    pub dependent_slice_segments_enabled: bool,
    /// `output_flag_present_flag`.
    pub output_flag_present: bool,
    /// `num_extra_slice_header_bits`.
    pub num_extra_slice_header_bits: u8,
    /// `sign_data_hiding_enabled_flag`.
    pub sign_data_hiding: bool,
    /// `cabac_init_present_flag`.
    pub cabac_init_present: bool,
    /// `num_ref_idx_l0_default_active_minus1`.
    pub num_ref_idx_l0_default_active_minus1: u8,
    /// `num_ref_idx_l1_default_active_minus1`.
    pub num_ref_idx_l1_default_active_minus1: u8,
    /// `init_qp_minus26`.
    pub init_qp_minus26: i8,
    /// `constrained_intra_pred_flag`.
    pub constrained_intra_pred: bool,
    /// `transform_skip_enabled_flag`.
    pub transform_skip_enabled: bool,
    /// `cu_qp_delta_enabled_flag`.
    pub cu_qp_delta_enabled: bool,
    /// `diff_cu_qp_delta_depth` (0 when `cu_qp_delta_enabled` is off).
    pub diff_cu_qp_delta_depth: u8,
    /// `pps_cb_qp_offset`.
    pub cb_qp_offset: i8,
    /// `pps_cr_qp_offset`.
    pub cr_qp_offset: i8,
    /// `pps_slice_chroma_qp_offsets_present_flag`.
    pub slice_chroma_qp_offsets_present: bool,
    /// `weighted_pred_flag`.
    pub weighted_pred: bool,
    /// `weighted_bipred_flag`.
    pub weighted_bipred: bool,
    /// `transquant_bypass_enabled_flag`.
    pub transquant_bypass_enabled: bool,
    /// `tiles_enabled_flag`.
    pub tiles_enabled: bool,
    /// `entropy_coding_sync_enabled_flag`.
    pub entropy_coding_sync_enabled: bool,
    /// `num_tile_columns_minus1` (0 without tiles).
    pub num_tile_columns_minus1: u8,
    /// `num_tile_rows_minus1` (0 without tiles).
    pub num_tile_rows_minus1: u8,
    /// `uniform_spacing_flag`.
    pub uniform_spacing: bool,
    /// `column_width_minus1` for the first `num_tile_columns_minus1` columns.
    pub column_width_minus1: [u16; MAX_TILE_COLUMNS],
    /// `row_height_minus1` for the first `num_tile_rows_minus1` rows.
    pub row_height_minus1: [u16; MAX_TILE_ROWS],
    /// `loop_filter_across_tiles_enabled_flag`.
    pub loop_filter_across_tiles: bool,
    /// `pps_loop_filter_across_slices_enabled_flag`.
    pub loop_filter_across_slices: bool,
    /// `deblocking_filter_control_present_flag`.
    pub deblocking_filter_control_present: bool,
    /// `deblocking_filter_override_enabled_flag`.
    pub deblocking_filter_override_enabled: bool,
    /// `pps_deblocking_filter_disabled_flag`.
    pub deblocking_filter_disabled: bool,
    /// `pps_beta_offset_div2`.
    pub beta_offset_div2: i8,
    /// `pps_tc_offset_div2`.
    pub tc_offset_div2: i8,
    /// `pps_scaling_list_data_present_flag` (the lists are read past, not kept).
    pub scaling_list_data_present: bool,
    /// `lists_modification_present_flag`.
    pub lists_modification_present: bool,
    /// `log2_parallel_merge_level_minus2`.
    pub log2_parallel_merge_level_minus2: u8,
    /// `slice_segment_header_extension_present_flag`.
    pub slice_header_extension_present: bool,
}

/// Parses a PPS NAL unit's RBSP (header included).
pub fn parse_pps(rbsp: &[u8]) -> Result<Pps> {
    let mut r = BitReader::new(rbsp);
    r.skip(HEADER_BYTES * 8)?;
    let mut p = Pps {
        id: r.ue_max(63, "pps_pic_parameter_set_id")? as u8,
        sps_id: r.ue_max(15, "pps_seq_parameter_set_id")? as u8,
        ..Pps::default()
    };
    p.dependent_slice_segments_enabled = r.flag()?;
    p.output_flag_present = r.flag()?;
    p.num_extra_slice_header_bits = r.bits(3)? as u8;
    p.sign_data_hiding = r.flag()?;
    p.cabac_init_present = r.flag()?;
    p.num_ref_idx_l0_default_active_minus1 =
        r.ue_max(14, "num_ref_idx_l0_default_active_minus1")? as u8;
    p.num_ref_idx_l1_default_active_minus1 =
        r.ue_max(14, "num_ref_idx_l1_default_active_minus1")? as u8;
    // -(26 + QpBdOffsetY) to 25, with QpBdOffsetY at most 48 for 16-bit video.
    p.init_qp_minus26 = r.se_range(-74, 25, "init_qp_minus26")? as i8;
    p.constrained_intra_pred = r.flag()?;
    p.transform_skip_enabled = r.flag()?;
    p.cu_qp_delta_enabled = r.flag()?;
    if p.cu_qp_delta_enabled {
        p.diff_cu_qp_delta_depth = r.ue_max(3, "diff_cu_qp_delta_depth")? as u8;
    }
    p.cb_qp_offset = r.se_range(-12, 12, "pps_cb_qp_offset")? as i8;
    p.cr_qp_offset = r.se_range(-12, 12, "pps_cr_qp_offset")? as i8;
    p.slice_chroma_qp_offsets_present = r.flag()?;
    p.weighted_pred = r.flag()?;
    p.weighted_bipred = r.flag()?;
    p.transquant_bypass_enabled = r.flag()?;
    p.tiles_enabled = r.flag()?;
    p.entropy_coding_sync_enabled = r.flag()?;
    if p.tiles_enabled {
        p.num_tile_columns_minus1 =
            r.ue_max(MAX_TILE_COLUMNS as u32 - 1, "num_tile_columns_minus1")? as u8;
        p.num_tile_rows_minus1 = r.ue_max(MAX_TILE_ROWS as u32 - 1, "num_tile_rows_minus1")? as u8;
        p.uniform_spacing = r.flag()?;
        if !p.uniform_spacing {
            let columns = p.num_tile_columns_minus1 as usize;
            for width in p.column_width_minus1.iter_mut().take(columns) {
                *width = r.ue_max(0xffff, "column_width_minus1")? as u16;
            }
            let rows = p.num_tile_rows_minus1 as usize;
            for height in p.row_height_minus1.iter_mut().take(rows) {
                *height = r.ue_max(0xffff, "row_height_minus1")? as u16;
            }
        }
        p.loop_filter_across_tiles = r.flag()?;
    }
    p.loop_filter_across_slices = r.flag()?;
    p.deblocking_filter_control_present = r.flag()?;
    if p.deblocking_filter_control_present {
        p.deblocking_filter_override_enabled = r.flag()?;
        p.deblocking_filter_disabled = r.flag()?;
        if !p.deblocking_filter_disabled {
            p.beta_offset_div2 = r.se_range(-6, 6, "pps_beta_offset_div2")? as i8;
            p.tc_offset_div2 = r.se_range(-6, 6, "pps_tc_offset_div2")? as i8;
        }
    }
    p.scaling_list_data_present = r.flag()?;
    if p.scaling_list_data_present {
        skip_scaling_list_data(&mut r)?;
    }
    p.lists_modification_present = r.flag()?;
    p.log2_parallel_merge_level_minus2 = r.ue_max(0xff, "log2_parallel_merge_level_minus2")? as u8;
    p.slice_header_extension_present = r.flag()?;
    Ok(p)
}
