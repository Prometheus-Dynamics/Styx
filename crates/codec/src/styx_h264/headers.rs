//! Parameter sets and slice headers (7.3.2.1, 7.3.2.2, 7.3.3) for the encoder's one shape:
//! Baseline, frames only, one reference frame, `pic_order_cnt_type` 2, 4-bit `frame_num`,
//! one slice per picture, the deblocking filter off.

use super::bits::Bits;

/// `log2_max_frame_num`.
pub(super) const LOG2_MAX_FRAME_NUM: u32 = 4;

/// The smallest level whose frame size limit (`MaxFS`, table A-1) takes `mbs`, at least 3.0.
fn level_idc(mbs: usize) -> u32 {
    match mbs {
        0..=1620 => 30,
        1621..=3600 => 31,
        3601..=5120 => 32,
        5121..=8192 => 40,
        8193..=22080 => 50,
        _ => 51,
    }
}

/// The sequence parameter set for a `width` x `height` (even) picture coded as `mb_w` x
/// `mb_h` macroblocks, cropped back to the picture.
pub(super) fn sps(b: &mut Bits, (width, height): (usize, usize), (mb_w, mb_h): (usize, usize)) {
    b.put(66, 8); // profile_idc: Baseline
    b.put(0b1100_0000, 8); // constraint_set0 and 1: also Constrained Baseline / Main
    b.put(level_idc(mb_w * mb_h), 8);
    b.ue(0); // seq_parameter_set_id
    b.ue(LOG2_MAX_FRAME_NUM - 4);
    b.ue(2); // pic_order_cnt_type
    b.ue(1); // max_num_ref_frames
    b.flag(false); // gaps_in_frame_num_value_allowed_flag
    b.ue(mb_w as u32 - 1);
    b.ue(mb_h as u32 - 1);
    b.flag(true); // frame_mbs_only_flag
    b.flag(true); // direct_8x8_inference_flag
    let (right, bottom) = ((mb_w * 16 - width) / 2, (mb_h * 16 - height) / 2);
    b.flag(right + bottom > 0); // frame_cropping_flag
    if right + bottom > 0 {
        b.ue(0);
        b.ue(right as u32);
        b.ue(0);
        b.ue(bottom as u32);
    }
    b.flag(false); // vui_parameters_present_flag
    b.trailing();
}

/// The picture parameter set: CAVLC, one slice group, QP 26 base, deblocking control on.
pub(super) fn pps(b: &mut Bits) {
    b.ue(0); // pic_parameter_set_id
    b.ue(0); // seq_parameter_set_id
    b.flag(false); // entropy_coding_mode_flag
    b.flag(false); // bottom_field_pic_order_in_frame_present_flag
    b.ue(0); // num_slice_groups_minus1
    b.ue(0); // num_ref_idx_l0_default_active_minus1
    b.ue(0); // num_ref_idx_l1_default_active_minus1
    b.flag(false); // weighted_pred_flag
    b.put(0, 2); // weighted_bipred_idc
    b.se(0); // pic_init_qp_minus26
    b.se(0); // pic_init_qs_minus26
    b.se(0); // chroma_qp_index_offset
    b.flag(true); // deblocking_filter_control_present_flag
    b.flag(false); // constrained_intra_pred_flag
    b.flag(false); // redundant_pic_cnt_present_flag
    b.trailing();
}

/// A slice header: the whole picture, `idr` (with `idr_pic_id`) or P, at `qp`, the
/// deblocking filter off.
pub(super) fn slice_header(b: &mut Bits, idr: Option<u32>, frame_num: u32, qp: u8) {
    b.ue(0); // first_mb_in_slice
    b.ue(if idr.is_some() { 7 } else { 5 }); // slice_type: all I / all P
    b.ue(0); // pic_parameter_set_id
    b.put(frame_num, LOG2_MAX_FRAME_NUM);
    match idr {
        Some(id) => b.ue(id),
        None => {
            b.flag(false); // num_ref_idx_active_override_flag
            b.flag(false); // ref_pic_list_modification_flag_l0
        }
    }
    // dec_ref_pic_marking: every picture is a reference.
    if idr.is_some() {
        b.flag(false); // no_output_of_prior_pics_flag
        b.flag(false); // long_term_reference_flag
    } else {
        b.flag(false); // adaptive_ref_pic_marking_mode_flag
    }
    b.se(i32::from(qp) - 26); // slice_qp_delta
    b.ue(1); // disable_deblocking_filter_idc
}
