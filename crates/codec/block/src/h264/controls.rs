//! The block's controls for an H.264 slice: the stateless V4L2 structures of
//! `lemnos_linux_sys::v4l2::h264`, filled from the parsed headers.
//!
//! A slice's job carries, in this order: the decode mode (slice based: one slice per job), the
//! start code (Annex B: the buffer holds the NAL unit with its start code), the SPS, the PPS, the
//! scaling matrix when one applies, the prediction weights when the slice has a table, the slice
//! parameters and the decode parameters. The decode parameters carry the decoded picture buffer:
//! entry `i` is the frame in DPB slot `i`, and its `reference_ts` is the stamp of the job that
//! decoded that frame. A reference list names a DPB entry by its index, so a slice names its
//! reference pictures by stamp and not by any buffer.
//!
//! Each value is narrowed with a check into the control's field: a header value that does not
//! fit is refused, never truncated. The scaling lists are converted from the bitstream's zigzag
//! order to the raster order the structure takes, with the fall-back rules of Table 7-2 applied.
//!
//! Provenance: written for Styx from ITU-T H.264 (7.3, 7.4, 8.5.6, Tables 7-2 to 7-4). The
//! structures' layout is Lemnos's, checked against the kernel header at its lock.

use lemnos_linux_sys::v4l2::h264 as sys;
use styx_codec_api::CodecError;

use crate::bitstream::raw_index;
use crate::h264::dpb::{Dpb, RefLists};
use crate::h264::poc::Poc;
use crate::h264::ps::{Pps, ScalingLists, Sps};
use crate::h264::slice::{SliceHeader, SliceKind};

/// The length of the Annex B start code in front of the NAL unit in the job's buffer.
pub const START_CODE_LEN: usize = 3;

/// The controls a slice's job carries, as (control id, bytes) in the order the block gets them.
/// Its buffers are reused from slice to slice.
#[derive(Clone, Debug, Default)]
pub struct Controls {
    bytes: Vec<u8>,
    items: Vec<(u32, usize, usize)>,
}

impl Controls {
    /// Empties the controls, keeping their buffers.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.items.clear();
    }

    /// Adds a control with `bytes`.
    pub fn push(&mut self, tag: u32, bytes: &[u8]) {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(bytes);
        self.items.push((tag, start, self.bytes.len()));
    }

    /// The controls, in order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &[u8])> + '_ {
        self.items
            .iter()
            .map(|&(tag, start, end)| (tag, &self.bytes[start..end]))
    }

    /// The number of controls.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there are no controls.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// What the controls of one slice are written from: its parameter sets, its header and the NAL
/// unit's bytes it was parsed from, the picture's order counts, the reference picture lists
/// (DPB slots), and the DPB they name.
#[derive(Clone, Copy, Debug)]
pub struct SliceView<'a> {
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub header: &'a SliceHeader,
    /// The NAL unit as the stream has it, emulation prevention bytes included.
    pub raw: &'a [u8],
    /// The picture's order counts, before any memory management operation 5.
    pub poc: Poc,
    pub lists: &'a RefLists,
    pub dpb: &'a Dpb,
}

/// `value` as the field `T` takes it, or a refusal when it does not fit.
fn fit<T: TryFrom<i64>, S: TryInto<i64>>(value: S, what: &'static str) -> Result<T, CodecError> {
    value
        .try_into()
        .ok()
        .and_then(|v| T::try_from(v).ok())
        .ok_or_else(|| CodecError::Rejected(format!("{what} does not fit")))
}

/// The field's value less `by`, refused when it would go below zero.
fn less<T: Into<i64>>(value: T, by: i64, what: &'static str) -> Result<i64, CodecError> {
    let v = value.into() - by;
    if v < 0 {
        return Err(CodecError::Rejected(format!("{what} is out of range")));
    }
    Ok(v)
}

/// `V4L2_CID_STATELESS_H264_SPS` for `s` (7.4.2.1.1).
pub fn sps_control(s: &Sps) -> Result<sys::Sps, CodecError> {
    use sys::sps_flags as f;
    let flags_on = [
        (s.separate_colour_plane, f::SEPARATE_COLOUR_PLANE),
        (s.transform_bypass, f::QPPRIME_Y_ZERO_TRANSFORM_BYPASS),
        (
            s.delta_pic_order_always_zero,
            f::DELTA_PIC_ORDER_ALWAYS_ZERO,
        ),
        (s.gaps_allowed, f::GAPS_IN_FRAME_NUM_VALUE_ALLOWED),
        (s.frame_mbs_only, f::FRAME_MBS_ONLY),
        (s.mb_adaptive_frame_field, f::MB_ADAPTIVE_FRAME_FIELD),
        (s.direct_8x8_inference, f::DIRECT_8X8_INFERENCE),
    ];
    let flags = flags_on
        .iter()
        .filter(|(on, _)| *on)
        .fold(0u32, |acc, (_, bit)| acc | bit);
    if s.offset_for_ref_frame.len() > 255 {
        return Err(CodecError::Rejected("offset_for_ref_frame".into()));
    }
    let mut offsets = [0i32; 255];
    offsets[..s.offset_for_ref_frame.len()].copy_from_slice(&s.offset_for_ref_frame);
    Ok(sys::Sps {
        profile_idc: s.profile_idc,
        constraint_set_flags: s.constraint_flags,
        level_idc: s.level_idc,
        seq_parameter_set_id: s.id,
        chroma_format_idc: s.chroma_format_idc,
        bit_depth_luma_minus8: s.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: s.bit_depth_chroma_minus8,
        log2_max_frame_num_minus4: fit(
            less(s.log2_max_frame_num, 4, "log2_max_frame_num")?,
            "log2_max_frame_num",
        )?,
        pic_order_cnt_type: s.poc_type,
        log2_max_pic_order_cnt_lsb_minus4: fit(
            less(s.log2_max_poc_lsb, 4, "log2_max_pic_order_cnt_lsb")?,
            "log2_max_pic_order_cnt_lsb",
        )?,
        max_num_ref_frames: s.max_num_ref_frames,
        num_ref_frames_in_pic_order_cnt_cycle: fit(s.offset_for_ref_frame.len(), "offset count")?,
        offset_for_ref_frame: offsets,
        offset_for_non_ref_pic: s.offset_for_non_ref_pic,
        offset_for_top_to_bottom_field: s.offset_for_top_to_bottom_field,
        pic_width_in_mbs_minus1: fit(less(s.width_mbs, 1, "width")?, "width")?,
        pic_height_in_map_units_minus1: fit(less(s.height_map_units, 1, "height")?, "height")?,
        flags,
    })
}

/// `V4L2_CID_STATELESS_H264_PPS` for `p`. `scaling` says whether a scaling matrix applies to
/// the slices (from [`scaling_matrix`]).
pub fn pps_control(p: &Pps, scaling: bool) -> Result<sys::Pps, CodecError> {
    use sys::pps_flags as f;
    let flags_on = [
        (p.cabac, f::ENTROPY_CODING_MODE),
        (
            p.bottom_field_pic_order_present,
            f::BOTTOM_FIELD_PIC_ORDER_IN_FRAME_PRESENT,
        ),
        (p.weighted_pred, f::WEIGHTED_PRED),
        (
            p.deblocking_filter_control_present,
            f::DEBLOCKING_FILTER_CONTROL_PRESENT,
        ),
        (p.constrained_intra_pred, f::CONSTRAINED_INTRA_PRED),
        (p.redundant_pic_cnt_present, f::REDUNDANT_PIC_CNT_PRESENT),
        (p.transform_8x8_mode, f::TRANSFORM_8X8_MODE),
        (scaling, f::SCALING_MATRIX_PRESENT),
    ];
    let flags = flags_on
        .iter()
        .filter(|(on, _)| *on)
        .fold(0u16, |acc, (_, bit)| acc | bit);
    Ok(sys::Pps {
        pic_parameter_set_id: p.id,
        seq_parameter_set_id: p.sps_id,
        num_slice_groups_minus1: 0,
        num_ref_idx_l0_default_active_minus1: fit(
            less(p.num_ref_idx_default[0], 1, "num_ref_idx_l0_default")?,
            "num_ref_idx_l0_default",
        )?,
        num_ref_idx_l1_default_active_minus1: fit(
            less(p.num_ref_idx_default[1], 1, "num_ref_idx_l1_default")?,
            "num_ref_idx_l1_default",
        )?,
        weighted_bipred_idc: p.weighted_bipred_idc,
        pic_init_qp_minus26: fit(p.pic_init_qp - 26, "pic_init_qp")?,
        pic_init_qs_minus26: fit(p.pic_init_qs - 26, "pic_init_qs")?,
        chroma_qp_index_offset: fit(p.chroma_qp_index_offset, "chroma_qp_index_offset")?,
        second_chroma_qp_index_offset: fit(
            p.second_chroma_qp_index_offset,
            "second_chroma_qp_index_offset",
        )?,
        flags,
    })
}

/// Zigzag position to raster index, 4x4 (8.5.6, Table 8-13): `ZIGZAG_4X4[k]` is the raster
/// index of the `k`th value of a list.
const ZIGZAG_4X4: [u8; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

/// Zigzag position to raster index, 8x8 (8.5.7, Table 8-13).
const ZIGZAG_8X8: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Table 7-3: the default 4x4 lists, in zigzag order.
const DEFAULT_4X4_INTRA: [u8; 16] = [
    6, 13, 13, 20, 20, 20, 28, 28, 28, 28, 32, 32, 32, 37, 37, 42,
];
const DEFAULT_4X4_INTER: [u8; 16] = [
    10, 14, 14, 20, 20, 20, 24, 24, 24, 24, 27, 27, 27, 30, 30, 34,
];

/// Table 7-4: the default 8x8 lists, in zigzag order.
const DEFAULT_8X8_INTRA: [u8; 64] = [
    6, 10, 10, 13, 11, 13, 16, 16, 16, 16, 18, 18, 18, 18, 18, 23, 23, 23, 23, 23, 23, 25, 25, 25,
    25, 25, 25, 25, 27, 27, 27, 27, 27, 27, 27, 27, 29, 29, 29, 29, 29, 29, 29, 31, 31, 31, 31, 31,
    31, 33, 33, 33, 33, 33, 36, 36, 36, 36, 38, 38, 38, 40, 40, 42,
];
const DEFAULT_8X8_INTER: [u8; 64] = [
    9, 13, 13, 15, 13, 15, 17, 17, 17, 17, 19, 19, 19, 19, 19, 21, 21, 21, 21, 21, 21, 22, 22, 22,
    22, 22, 22, 22, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27, 27,
    27, 28, 28, 28, 28, 28, 30, 30, 30, 30, 32, 32, 32, 33, 33, 35,
];

/// The twelve lists of a scaling set in the bitstream's order (Table 7-2): six 4x4 lists (Intra
/// Y, Cb, Cr, then Inter Y, Cb, Cr), then six 8x8 (Intra Y, Inter Y, Intra Cb, Inter Cb, Intra
/// Cr, Inter Cr). A 4x4 list uses the first 16 values, in zigzag order.
type Lists = [[u8; 64]; 12];

/// The default list of list `i` (Table 7-2's default for an absent or defaulted list).
fn default_list(i: usize) -> &'static [u8] {
    match i {
        0..=2 => &DEFAULT_4X4_INTRA,
        3..=5 => &DEFAULT_4X4_INTER,
        6 | 8 | 10 => &DEFAULT_8X8_INTRA,
        _ => &DEFAULT_8X8_INTER,
    }
}

/// Resolves one parameter set's lists. A list the set does not carry falls back (Table 7-2):
/// rule A takes the default for lists 0, 3, 6 and 7 and the previous list of its kind for the
/// others; rule B, for a PPS in a SPS that has a matrix, takes the SPS's list for 0, 3, 6 and 7.
fn resolve(set: &ScalingLists, base: Option<&Lists>) -> Lists {
    let mut out: Lists = [[0; 64]; 12];
    for i in 0..12 {
        let len = if i < 6 { 16 } else { 64 };
        if set.present[i] {
            let src: &[u8] = if set.use_default[i] {
                default_list(i)
            } else {
                &set.values[i][..len]
            };
            out[i][..len].copy_from_slice(src);
            continue;
        }
        match (i, base) {
            (0 | 3 | 6 | 7, Some(b)) => out[i] = b[i],
            (0 | 3 | 6 | 7, None) => out[i][..len].copy_from_slice(default_list(i)),
            (1 | 2 | 4 | 5, _) => out[i] = out[i - 1],
            _ => out[i] = out[i - 2],
        }
    }
    out
}

/// The scaling matrix that applies to a slice, in raster order, or `None` for a flat one: the
/// PPS's matrix when it has one, else the SPS's.
pub fn scaling_matrix(sps: &Sps, pps: &Pps) -> Option<sys::ScalingMatrix> {
    let from_sps = sps.scaling.as_deref().map(|s| resolve(s, None));
    let effective = match pps.scaling.as_deref() {
        Some(p) => Some(resolve(p, from_sps.as_ref())),
        None => from_sps,
    }?;
    let mut matrix = sys::ScalingMatrix::default();
    for list in 0..6 {
        for (k, &pos) in ZIGZAG_4X4.iter().enumerate() {
            matrix.scaling_list_4x4[list][usize::from(pos)] = effective[list][k];
        }
    }
    for list in 0..6 {
        for (k, &pos) in ZIGZAG_8X8.iter().enumerate() {
            matrix.scaling_list_8x8[list][usize::from(pos)] = effective[6 + list][k];
        }
    }
    Some(matrix)
}

/// `V4L2_CID_STATELESS_H264_PRED_WEIGHTS` for a slice's table: an entry the stream leaves out
/// takes the default weight (`2^denominator`) and offset 0 (7.4.3.2).
fn pred_weights(h: &SliceHeader) -> Result<Option<sys::PredWeights>, CodecError> {
    let Some(w) = h.weights.as_deref() else {
        return Ok(None);
    };
    let luma_default = 1i64 << w.luma_log2_denom;
    let chroma_default = 1i64 << w.chroma_log2_denom;
    let mut out = sys::PredWeights {
        luma_log2_weight_denom: u16::from(w.luma_log2_denom),
        chroma_log2_weight_denom: u16::from(w.chroma_log2_denom),
        weight_factors: [sys::WeightFactors::default(); 2],
    };
    for (list, factors) in out.weight_factors.iter_mut().enumerate() {
        for i in 0..sys::REF_LIST_LEN {
            let entry = w.entries[list][i];
            let (lw, lo) = entry.luma.unwrap_or((luma_default as i32, 0));
            factors.luma_weight[i] = fit(lw, "luma weight")?;
            factors.luma_offset[i] = fit(lo, "luma offset")?;
            let chroma = entry
                .chroma
                .unwrap_or([(chroma_default as i32, 0), (chroma_default as i32, 0)]);
            factors.chroma_weight[i] = [
                fit(chroma[0].0, "chroma weight")?,
                fit(chroma[1].0, "chroma weight")?,
            ];
            factors.chroma_offset[i] = [
                fit(chroma[0].1, "chroma offset")?,
                fit(chroma[1].1, "chroma offset")?,
            ];
        }
    }
    Ok(Some(out))
}

/// `V4L2_H264_SLICE_TYPE_*` for a slice's kind.
const fn slice_type(kind: SliceKind) -> u8 {
    match kind {
        SliceKind::P => sys::SLICE_TYPE_P,
        SliceKind::B => sys::SLICE_TYPE_B,
        SliceKind::I => sys::SLICE_TYPE_I,
        SliceKind::SP => sys::SLICE_TYPE_SP,
        SliceKind::SI => sys::SLICE_TYPE_SI,
    }
}

/// `V4L2_CID_STATELESS_H264_SLICE_PARAMS` for a slice. `header_bit_size` counts from the start
/// of the job's buffer (the start code first) to the slice data, through the emulation
/// prevention bytes the stream has.
fn slice_params(view: &SliceView<'_>) -> Result<sys::SliceParams, CodecError> {
    use sys::slice_flags as f;
    let h = view.header;
    let rbsp_byte = h.data_bit_offset / 8;
    let raw_byte = raw_index(view.raw, rbsp_byte)
        .ok_or_else(|| CodecError::Rejected("the slice data is past the NAL unit".into()))?;
    let bits = (START_CODE_LEN + raw_byte) * 8 + h.data_bit_offset % 8;
    let mut p = sys::SliceParams {
        header_bit_size: fit(bits, "the slice header size")?,
        first_mb_in_slice: h.first_mb_in_slice,
        slice_type: slice_type(h.kind),
        colour_plane_id: h.colour_plane_id,
        redundant_pic_cnt: fit(h.redundant_pic_cnt, "redundant_pic_cnt")?,
        cabac_init_idc: h.cabac_init_idc,
        slice_qp_delta: fit(h.qp_delta, "slice_qp_delta")?,
        slice_qs_delta: fit(h.qs_delta, "slice_qs_delta")?,
        disable_deblocking_filter_idc: h.disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2: fit(h.alpha_c0_offset_div2, "slice_alpha_c0_offset_div2")?,
        slice_beta_offset_div2: fit(h.beta_offset_div2, "slice_beta_offset_div2")?,
        num_ref_idx_l0_active_minus1: h.num_ref_idx_active[0].saturating_sub(1),
        num_ref_idx_l1_active_minus1: h.num_ref_idx_active[1].saturating_sub(1),
        ..sys::SliceParams::default()
    };
    for (i, &slot) in view.lists.list(0).iter().enumerate() {
        p.ref_pic_list0[i] = sys::Reference {
            fields: sys::FRAME_REF,
            index: slot,
        };
    }
    for (i, &slot) in view.lists.list(1).iter().enumerate() {
        p.ref_pic_list1[i] = sys::Reference {
            fields: sys::FRAME_REF,
            index: slot,
        };
    }
    if h.direct_spatial_mv_pred {
        p.flags |= f::DIRECT_SPATIAL_MV_PRED;
    }
    if h.sp_for_switch {
        p.flags |= f::SP_FOR_SWITCH;
    }
    Ok(p)
}

/// `V4L2_CID_STATELESS_H264_DECODE_PARAMS` for a picture's slice: the DPB, with each frame's
/// stamp as its `reference_ts`, and the slice-invariant syntax elements.
fn decode_params(view: &SliceView<'_>) -> Result<sys::DecodeParams, CodecError> {
    use sys::decode_flags as f;
    let h = view.header;
    let mut p = sys::DecodeParams::default();
    if !h.is_idr() {
        let max = view.sps.max_frame_num();
        for (slot, frame) in view.dpb.frames() {
            let in_list = view.lists.list(0).contains(&slot) || view.lists.list(1).contains(&slot);
            let pic_num = view
                .dpb
                .pic_num(slot, h.frame_num, max)
                .ok_or_else(|| CodecError::Rejected("an empty DPB slot".into()))?;
            let mut flags = sys::dpb_flags::VALID;
            if in_list {
                flags |= sys::dpb_flags::ACTIVE;
            }
            if frame.long_term.is_some() {
                flags |= sys::dpb_flags::LONG_TERM;
            }
            p.dpb[usize::from(slot)] = sys::DpbEntry {
                reference_ts: frame.stamp_ns,
                pic_num: fit::<i32, i64>(pic_num, "PicNum")? as u32,
                frame_num: fit(frame.frame_num, "frame_num")?,
                fields: sys::FRAME_REF,
                reserved: [0; 5],
                top_field_order_cnt: frame.poc.top,
                bottom_field_order_cnt: frame.poc.bottom,
                flags,
            };
        }
    }
    p.nal_ref_idc = u16::from(h.nal_ref_idc);
    p.frame_num = fit(h.frame_num, "frame_num")?;
    p.top_field_order_cnt = view.poc.top;
    p.bottom_field_order_cnt = view.poc.bottom;
    p.idr_pic_id = fit(h.idr_pic_id, "idr_pic_id")?;
    p.pic_order_cnt_lsb = fit(h.poc_lsb, "pic_order_cnt_lsb")?;
    p.delta_pic_order_cnt_bottom = h.delta_poc_bottom;
    p.delta_pic_order_cnt0 = h.delta_poc[0];
    p.delta_pic_order_cnt1 = h.delta_poc[1];
    p.dec_ref_pic_marking_bit_size = h.marking_bits;
    p.pic_order_cnt_bit_size = h.poc_bits;
    if h.is_idr() {
        p.flags |= f::IDR_PIC;
    }
    match h.kind {
        SliceKind::P | SliceKind::SP => p.flags |= f::PFRAME,
        SliceKind::B => p.flags |= f::BFRAME,
        SliceKind::I | SliceKind::SI => {}
    }
    Ok(p)
}

/// Writes the block's controls for a slice into `out`, which the caller has cleared. An error
/// refuses the slice: a value that does not fit its structure, or a slice the controls cannot
/// express.
pub fn write(view: &SliceView<'_>, out: &mut Controls) -> Result<(), CodecError> {
    if view.header.redundant_pic_cnt != 0 {
        return Err(CodecError::Rejected(
            "a redundant slice is not decoded".into(),
        ));
    }
    let matrix = scaling_matrix(view.sps, view.pps);
    let sps = sps_control(view.sps)?;
    let pps = pps_control(view.pps, matrix.is_some())?;
    out.push(
        sys::CID_DECODE_MODE,
        &sys::DECODE_MODE_SLICE_BASED.to_le_bytes(),
    );
    out.push(sys::CID_START_CODE, &sys::START_CODE_ANNEX_B.to_le_bytes());
    out.push(sys::CID_SPS, sps.as_bytes());
    out.push(sys::CID_PPS, pps.as_bytes());
    if let Some(matrix) = matrix {
        out.push(sys::CID_SCALING_MATRIX, matrix.as_bytes());
    }
    if let Some(weights) = pred_weights(view.header)? {
        out.push(sys::CID_PRED_WEIGHTS, weights.as_bytes());
    }
    out.push(sys::CID_SLICE_PARAMS, slice_params(view)?.as_bytes());
    out.push(sys::CID_DECODE_PARAMS, decode_params(view)?.as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_zigzag_scans_are_permutations() {
        let mut seen4 = [false; 16];
        for &pos in &ZIGZAG_4X4 {
            assert!(!std::mem::replace(&mut seen4[usize::from(pos)], true));
        }
        let mut seen8 = [false; 64];
        for &pos in &ZIGZAG_8X8 {
            assert!(!std::mem::replace(&mut seen8[usize::from(pos)], true));
        }
    }

    #[test]
    fn a_defaulted_list_is_the_default_matrix_in_raster_order() {
        let mut set = ScalingLists::default();
        set.present[0] = true;
        set.use_default[0] = true;
        let resolved = resolve(&set, None);
        assert_eq!(&resolved[0][..16], &DEFAULT_4X4_INTRA);
        // Absent lists 1 and 2 follow list 0; list 3 takes the inter default.
        assert_eq!(resolved[1], resolved[0]);
        assert_eq!(&resolved[3][..16], &DEFAULT_4X4_INTER);
        // The 8x8 Intra Y default, and the scan puts its first value at raster 0.
        assert_eq!(&resolved[6][..64], &DEFAULT_8X8_INTRA);
        let mut matrix = sys::ScalingMatrix::default();
        for (k, &pos) in ZIGZAG_4X4.iter().enumerate() {
            matrix.scaling_list_4x4[0][usize::from(pos)] = resolved[0][k];
        }
        assert_eq!(matrix.scaling_list_4x4[0][0], 6);
        assert_eq!(matrix.scaling_list_4x4[0][1], 13);
        assert_eq!(matrix.scaling_list_4x4[0][4], 13);
    }

    #[test]
    fn a_pps_matrix_falls_back_to_the_sps_under_rule_b() {
        let mut sps_set = ScalingLists::default();
        sps_set.present[0] = true;
        sps_set.values[0][..16].copy_from_slice(&[9; 16]);
        let base = resolve(&sps_set, None);
        let pps_set = ScalingLists::default();
        let resolved = resolve(&pps_set, Some(&base));
        // Rule B takes the SPS's list 0; the other absent lists follow their own rule.
        assert_eq!(resolved[0], base[0]);
        assert_eq!(resolved[1], resolved[0]);
        // Under rule A (no SPS matrix) list 0 is the default instead.
        let rule_a = resolve(&pps_set, None);
        assert_eq!(&rule_a[0][..16], &DEFAULT_4X4_INTRA);
    }

    #[test]
    fn a_value_that_does_not_fit_is_refused() {
        assert!(fit::<u8, i64>(256, "x").is_err());
        assert!(fit::<u8, i64>(255, "x").is_ok());
        assert!(less(3u8, 4, "x").is_err());
        assert!(fit::<u16, usize>(70_000, "x").is_err());
    }
}
