//! Stateless HEVC controls from parsed headers: the parameter sets, and an IDR
//! slice's slice and decode parameters, in the layout of
//! `lemnos_linux_sys::v4l2::hevc`. The headers come from this crate's parser
//! ([`crate::hevc`]); the job that carries the controls is built by
//! [`crate::work`] and run by the block.
//!
//! Provenance: moved from Lemnos `drivers/m2m-decode/src/hevc.rs` (lock 04d09360).
//!
//! Every value is narrowed with a check into the control's field: a header
//! value that does not fit is refused, never truncated.
//!
//! The buffer a stateless decoder takes for a slice is the NAL unit with its
//! start code in front (`START_CODE_ANNEX_B`), so the slice data's offset in the
//! buffer is the start code's length plus the NAL unit's offset to the data.

use std::io;

use crate::hevc;
use lemnos_linux_sys::v4l2::hevc as sys;

/// The length of an Annex B start code (`00 00 01`), in front of a NAL unit in
/// the buffer.
pub const START_CODE_LEN: usize = 3;

fn invalid(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what)
}

fn narrow_u8(value: u32, what: &'static str) -> io::Result<u8> {
    u8::try_from(value).map_err(|_| invalid(what))
}

fn narrow_i8(value: i32, what: &'static str) -> io::Result<i8> {
    i8::try_from(value).map_err(|_| invalid(what))
}

/// `V4L2_CID_STATELESS_HEVC_SPS` for `sps`. Fails when a value does not fit
/// the control's fields (a sub-layer latency above 255).
pub fn sps_control(s: &hevc::Sps) -> io::Result<sys::Sps> {
    use sys::sps_flags as f;
    let mut flags = 0;
    let mut set = |on: bool, bit: u64| {
        if on {
            flags |= bit;
        }
    };
    set(s.separate_colour_plane, f::SEPARATE_COLOUR_PLANE);
    set(s.scaling_list_enabled, f::SCALING_LIST_ENABLED);
    set(s.amp_enabled, f::AMP_ENABLED);
    set(s.sao_enabled, f::SAMPLE_ADAPTIVE_OFFSET);
    set(s.pcm_enabled, f::PCM_ENABLED);
    set(s.pcm_loop_filter_disabled, f::PCM_LOOP_FILTER_DISABLED);
    set(s.long_term_ref_pics_present, f::LONG_TERM_REF_PICS_PRESENT);
    set(s.temporal_mvp_enabled, f::TEMPORAL_MVP_ENABLED);
    set(s.strong_intra_smoothing, f::STRONG_INTRA_SMOOTHING_ENABLED);
    Ok(sys::Sps {
        video_parameter_set_id: s.vps_id,
        seq_parameter_set_id: s.id,
        pic_width_in_luma_samples: u16::try_from(s.width).map_err(|_| invalid("width"))?,
        pic_height_in_luma_samples: u16::try_from(s.height).map_err(|_| invalid("height"))?,
        bit_depth_luma_minus8: s.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: s.bit_depth_chroma_minus8,
        log2_max_pic_order_cnt_lsb_minus4: s.log2_max_poc_lsb_minus4,
        sps_max_dec_pic_buffering_minus1: s.max_dec_pic_buffering_minus1,
        sps_max_num_reorder_pics: s.max_num_reorder_pics,
        sps_max_latency_increase_plus1: narrow_u8(
            s.max_latency_increase_plus1,
            "sps_max_latency_increase_plus1",
        )?,
        log2_min_luma_coding_block_size_minus3: s.log2_min_cb_minus3,
        log2_diff_max_min_luma_coding_block_size: s.log2_diff_max_min_cb,
        log2_min_luma_transform_block_size_minus2: s.log2_min_tb_minus2,
        log2_diff_max_min_luma_transform_block_size: s.log2_diff_max_min_tb,
        max_transform_hierarchy_depth_inter: s.max_transform_hierarchy_depth_inter,
        max_transform_hierarchy_depth_intra: s.max_transform_hierarchy_depth_intra,
        pcm_sample_bit_depth_luma_minus1: s.pcm_bit_depth_luma_minus1,
        pcm_sample_bit_depth_chroma_minus1: s.pcm_bit_depth_chroma_minus1,
        log2_min_pcm_luma_coding_block_size_minus3: s.log2_min_pcm_cb_minus3,
        log2_diff_max_min_pcm_luma_coding_block_size: s.log2_diff_max_min_pcm,
        num_short_term_ref_pic_sets: s.num_short_term_ref_pic_sets,
        num_long_term_ref_pics_sps: s.num_long_term_ref_pics_sps,
        chroma_format_idc: s.chroma_format_idc,
        sps_max_sub_layers_minus1: s.max_sub_layers_minus1,
        reserved: [0; 6],
        flags,
    })
}

/// `V4L2_CID_STATELESS_HEVC_PPS` for `p`. Fails when a tile size does not fit
/// the control's byte fields, or a scaling list is present (not converted).
pub fn pps_control(p: &hevc::Pps) -> io::Result<sys::Pps> {
    use sys::pps_flags as f;
    if p.scaling_list_data_present {
        return Err(invalid("a PPS with scaling lists"));
    }
    let mut flags = 0;
    let mut set = |on: bool, bit: u64| {
        if on {
            flags |= bit;
        }
    };
    set(
        p.dependent_slice_segments_enabled,
        f::DEPENDENT_SLICE_SEGMENT_ENABLED,
    );
    set(p.output_flag_present, f::OUTPUT_FLAG_PRESENT);
    set(p.sign_data_hiding, f::SIGN_DATA_HIDING_ENABLED);
    set(p.cabac_init_present, f::CABAC_INIT_PRESENT);
    set(p.constrained_intra_pred, f::CONSTRAINED_INTRA_PRED);
    set(p.transform_skip_enabled, f::TRANSFORM_SKIP_ENABLED);
    set(p.cu_qp_delta_enabled, f::CU_QP_DELTA_ENABLED);
    set(
        p.slice_chroma_qp_offsets_present,
        f::SLICE_CHROMA_QP_OFFSETS_PRESENT,
    );
    set(p.weighted_pred, f::WEIGHTED_PRED);
    set(p.weighted_bipred, f::WEIGHTED_BIPRED);
    set(p.transquant_bypass_enabled, f::TRANSQUANT_BYPASS_ENABLED);
    set(p.tiles_enabled, f::TILES_ENABLED);
    set(
        p.entropy_coding_sync_enabled,
        f::ENTROPY_CODING_SYNC_ENABLED,
    );
    set(
        p.loop_filter_across_tiles,
        f::LOOP_FILTER_ACROSS_TILES_ENABLED,
    );
    set(
        p.loop_filter_across_slices,
        f::LOOP_FILTER_ACROSS_SLICES_ENABLED,
    );
    set(
        p.deblocking_filter_override_enabled,
        f::DEBLOCKING_FILTER_OVERRIDE_ENABLED,
    );
    set(p.deblocking_filter_disabled, f::DISABLE_DEBLOCKING_FILTER);
    set(p.lists_modification_present, f::LISTS_MODIFICATION_PRESENT);
    set(
        p.slice_header_extension_present,
        f::SLICE_SEGMENT_HEADER_EXTENSION_PRESENT,
    );
    set(
        p.deblocking_filter_control_present,
        f::DEBLOCKING_FILTER_CONTROL_PRESENT,
    );
    set(p.uniform_spacing, f::UNIFORM_SPACING);

    let mut column_width_minus1 = [0u8; 20];
    for (dst, src) in column_width_minus1.iter_mut().zip(p.column_width_minus1) {
        *dst = narrow_u8(u32::from(src), "column_width_minus1")?;
    }
    let mut row_height_minus1 = [0u8; 22];
    for (dst, src) in row_height_minus1.iter_mut().zip(p.row_height_minus1) {
        *dst = narrow_u8(u32::from(src), "row_height_minus1")?;
    }
    Ok(sys::Pps {
        pic_parameter_set_id: p.id,
        num_extra_slice_header_bits: p.num_extra_slice_header_bits,
        num_ref_idx_l0_default_active_minus1: p.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1: p.num_ref_idx_l1_default_active_minus1,
        init_qp_minus26: p.init_qp_minus26,
        diff_cu_qp_delta_depth: p.diff_cu_qp_delta_depth,
        pps_cb_qp_offset: p.cb_qp_offset,
        pps_cr_qp_offset: p.cr_qp_offset,
        num_tile_columns_minus1: p.num_tile_columns_minus1,
        num_tile_rows_minus1: p.num_tile_rows_minus1,
        column_width_minus1,
        row_height_minus1,
        pps_beta_offset_div2: p.beta_offset_div2,
        pps_tc_offset_div2: p.tc_offset_div2,
        log2_parallel_merge_level_minus2: p.log2_parallel_merge_level_minus2,
        reserved: 0,
        flags,
    })
}

/// The slice and decode parameters of one IDR slice segment, with the buffer
/// that holds it.
#[derive(Clone, Copy, Debug)]
pub struct IdrSlice {
    /// `V4L2_CID_STATELESS_HEVC_SLICE_PARAMS` (one element).
    pub slice: sys::SliceParams,
    /// `V4L2_CID_STATELESS_HEVC_DECODE_PARAMS`.
    pub decode: sys::DecodeParams,
}

/// The slice parameters of an IDR slice segment, for the buffer holding its
/// NAL unit `raw` (see [`hevc::nal_units`]) with [`START_CODE_LEN`] bytes
/// of start code in front; `buffer_len` is the buffer's length in bytes.
/// `header` is that segment's parsed header, which
/// `hevc::parse_slice_header` read from the unescaped copy of `raw`.
pub fn idr_slice(
    header: &hevc::SliceHeader,
    raw: &[u8],
    unit_type: u8,
    temporal_id_plus1: u8,
    buffer_len: usize,
) -> io::Result<IdrSlice> {
    use sys::slice_flags as f;
    if !hevc::is_idr(unit_type) {
        return Err(invalid("a slice of a picture that is not IDR"));
    }
    let data_at =
        hevc::raw_index(raw, header.data_offset).ok_or_else(|| invalid("slice data offset"))?;
    let mut flags = 0;
    if header.sao_luma {
        flags |= f::SAO_LUMA;
    }
    if header.sao_chroma {
        flags |= f::SAO_CHROMA;
    }
    if header.deblocking_disabled {
        flags |= f::DEBLOCKING_FILTER_DISABLED;
    }
    if header.loop_filter_across_slices {
        flags |= f::LOOP_FILTER_ACROSS_SLICES_ENABLED;
    }
    if header.dependent {
        flags |= f::DEPENDENT_SLICE_SEGMENT;
    }
    let data_byte_offset = START_CODE_LEN + data_at;
    // The slice data's size in bits: from its offset to the end of the buffer.
    let bit_size = buffer_len
        .checked_sub(data_byte_offset)
        .ok_or_else(|| invalid("slice data offset past the buffer"))?
        .checked_mul(8)
        .and_then(|bits| u32::try_from(bits).ok())
        .ok_or_else(|| invalid("buffer size"))?;
    let slice = sys::SliceParams {
        bit_size,
        data_byte_offset: u32::try_from(data_byte_offset)
            .map_err(|_| invalid("slice data offset"))?,
        num_entry_point_offsets: header.num_entry_point_offsets,
        nal_unit_type: unit_type,
        nuh_temporal_id_plus1: temporal_id_plus1,
        slice_type: sys::SLICE_TYPE_I,
        colour_plane_id: header.colour_plane_id,
        slice_qp_delta: narrow_i8(header.qp_delta, "slice_qp_delta")?,
        slice_cb_qp_offset: narrow_i8(header.cb_qp_offset, "slice_cb_qp_offset")?,
        slice_cr_qp_offset: narrow_i8(header.cr_qp_offset, "slice_cr_qp_offset")?,
        slice_beta_offset_div2: narrow_i8(header.beta_offset_div2, "slice_beta_offset_div2")?,
        slice_tc_offset_div2: narrow_i8(header.tc_offset_div2, "slice_tc_offset_div2")?,
        slice_segment_addr: header.segment_address,
        flags,
        ..sys::SliceParams::default()
    };
    let mut decode_flags = sys::decode_flags::IRAP_PIC | sys::decode_flags::IDR_PIC;
    if header.no_output_of_prior_pics {
        decode_flags |= sys::decode_flags::NO_OUTPUT_OF_PRIOR;
    }
    let decode = sys::DecodeParams {
        flags: decode_flags,
        ..sys::DecodeParams::default()
    };
    Ok(IdrSlice { slice, decode })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &[u8] = include_bytes!("fixtures/idr320.h265");

    fn parsed() -> (
        hevc::Sps,
        hevc::Pps,
        hevc::NalUnit<'static>,
        hevc::SliceHeader,
    ) {
        let mut buf = [0u8; 4096];
        let (mut sps, mut pps, mut slice) = (None, None, None);
        for nal in hevc::nal_units(STREAM) {
            let n = hevc::unescape(nal.raw, &mut buf).unwrap();
            match nal.unit_type {
                33 => sps = Some(hevc::parse_sps(&buf[..n]).unwrap()),
                34 => pps = Some(hevc::parse_pps(&buf[..n]).unwrap()),
                19 => {
                    let h = hevc::parse_slice_header(
                        &buf[..n],
                        nal.unit_type,
                        sps.as_ref().unwrap(),
                        pps.as_ref().unwrap(),
                    )
                    .unwrap();
                    slice = Some((nal, h));
                }
                _ => {}
            }
        }
        let (nal, header) = slice.unwrap();
        (sps.unwrap(), pps.unwrap(), nal, header)
    }

    #[test]
    fn the_fixtures_headers_become_controls() {
        let (sps, pps, nal, header) = parsed();
        let s = sps_control(&sps).unwrap();
        assert_eq!(
            (s.pic_width_in_luma_samples, s.pic_height_in_luma_samples),
            (320, 240)
        );
        assert_eq!(s.chroma_format_idc, 1);
        assert_eq!(s.flags & sys::sps_flags::SCALING_LIST_ENABLED, 0);
        let p = pps_control(&pps).unwrap();
        assert_eq!(p.pic_parameter_set_id, 0);
        assert_eq!(p.flags & sys::pps_flags::TILES_ENABLED, 0);

        let buffer_len = START_CODE_LEN + nal.raw.len();
        let idr = idr_slice(
            &header,
            nal.raw,
            nal.unit_type,
            nal.temporal_id_plus1,
            buffer_len,
        )
        .unwrap();
        let at = hevc::raw_index(nal.raw, header.data_offset).unwrap();
        assert_eq!(idr.slice.data_byte_offset as usize, START_CODE_LEN + at);
        // The data runs to the end of the buffer: what the board's driver wants.
        assert_eq!(
            idr.slice.bit_size as usize,
            (buffer_len - START_CODE_LEN - at) * 8
        );
        assert_eq!(idr.slice.nal_unit_type, 19);
        assert_eq!(idr.slice.slice_type, sys::SLICE_TYPE_I);
        assert_ne!(idr.decode.flags & sys::decode_flags::IDR_PIC, 0);
        assert_ne!(idr.decode.flags & sys::decode_flags::IRAP_PIC, 0);
    }

    #[test]
    fn values_that_do_not_fit_their_fields_are_refused() {
        let (sps, pps, nal, header) = parsed();
        let wide = hevc::Sps {
            width: 70_000,
            ..sps
        };
        assert!(sps_control(&wide).is_err());
        let latent = hevc::Sps {
            max_latency_increase_plus1: 256,
            ..sps
        };
        assert!(sps_control(&latent).is_err());
        let scaled = hevc::Pps {
            scaling_list_data_present: true,
            ..pps
        };
        assert!(pps_control(&scaled).is_err());
        let tiled = hevc::Pps {
            column_width_minus1: [300; 20],
            ..pps
        };
        assert!(pps_control(&tiled).is_err());
        // A buffer shorter than the slice header, a non-IDR type, a QP out of range.
        assert!(idr_slice(&header, nal.raw, nal.unit_type, 1, 2).is_err());
        assert!(idr_slice(&header, nal.raw, 1, 1, 4096).is_err());
        let hot = hevc::SliceHeader {
            qp_delta: 200,
            ..header
        };
        assert!(idr_slice(&hot, nal.raw, nal.unit_type, 1, 4096).is_err());
        let past = hevc::SliceHeader {
            data_offset: nal.raw.len() + 10,
            ..header
        };
        assert!(idr_slice(&past, nal.raw, nal.unit_type, 1, 4096).is_err());
    }
}
