//! Unit tests of the H.264 parsers, the order counts and the reference tracker, on the openh264
//! fixture (a real Constrained Baseline stream) and on headers the test writes bit by bit.

use super::*;
use crate::bitstream::{Error, unescape};

const OPENH264: &[u8] = include_bytes!("fixtures/openh264_cbp.h264");

/// A bit writer for the syntax the tests build: `u(n)`, `ue`, `se`, and the trailing bits.
#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    n: usize,
}

impl Bits {
    fn bit(&mut self, b: bool) {
        if self.n.is_multiple_of(8) {
            self.out.push(0);
        }
        if b {
            let last = self.out.len() - 1;
            self.out[last] |= 0x80 >> (self.n % 8);
        }
        self.n += 1;
    }
    fn u(&mut self, bits: u32, v: u32) {
        for i in (0..bits).rev() {
            self.bit((v >> i) & 1 == 1);
        }
    }
    fn flag(&mut self, b: bool) {
        self.bit(b);
    }
    fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        for _ in 0..len - 1 {
            self.bit(false);
        }
        for i in (0..len).rev() {
            self.bit((x >> i) & 1 == 1);
        }
    }
    fn se(&mut self, v: i32) {
        let k = if v > 0 {
            2 * v as u32 - 1
        } else {
            (-2 * v) as u32
        };
        self.ue(k);
    }
    /// `rbsp_trailing_bits()` (a one bit and zeros to the byte boundary).
    fn finish(mut self, header: u8) -> Vec<u8> {
        self.bit(true);
        let mut out = vec![header];
        out.extend(self.out);
        out
    }
}

/// A NAL unit's RBSP with its header byte, from a writer.
fn rbsp(ref_idc: u8, unit_type: u8, body: Bits) -> Vec<u8> {
    body.finish((ref_idc << 5) | unit_type)
}

/// The Annex B bytes of an RBSP (start code, header, emulation prevention).
fn annexb(rbsp: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1];
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

fn unescaped(raw: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; raw.len()];
    let n = unescape(raw, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

/// The units of a stream, unescaped: (type, ref_idc, RBSP with header).
fn units(stream: &[u8]) -> Vec<(u8, u8, Vec<u8>)> {
    nal_units(stream)
        .map(|nal| (nal.unit_type, nal.nal_ref_idc, unescaped(nal.raw)))
        .collect()
}

/// A sequence parameter set the test writes.
#[derive(Clone)]
struct SpsSpec {
    profile: u32,
    chroma: u32,
    log2_frame_num: u32,
    poc: PocSpec,
    max_refs: u32,
    width_mbs: u32,
    height_map_units: u32,
    frame_mbs_only: bool,
    mbaff: bool,
    crop: Option<[u32; 4]>,
}

#[derive(Clone)]
enum PocSpec {
    Type0 {
        log2_lsb: u32,
    },
    Type1 {
        always_zero: bool,
        non_ref: i32,
        top_to_bottom: i32,
        cycle: Vec<i32>,
    },
    Type2,
}

impl SpsSpec {
    fn baseline(max_refs: u32, width_mbs: u32, height_map_units: u32) -> Self {
        Self {
            profile: 66,
            chroma: 1,
            log2_frame_num: 4,
            poc: PocSpec::Type0 { log2_lsb: 4 },
            max_refs,
            width_mbs,
            height_map_units,
            frame_mbs_only: true,
            mbaff: false,
            crop: None,
        }
    }

    fn rbsp(&self) -> Vec<u8> {
        let mut b = Bits::default();
        b.u(8, self.profile);
        b.u(8, 0);
        b.u(8, 30);
        b.ue(0);
        if matches!(self.profile, 100 | 110 | 122 | 244) {
            b.ue(self.chroma);
            if self.chroma == 3 {
                b.flag(false);
            }
            b.ue(0);
            b.ue(0);
            b.flag(false);
            b.flag(false);
        }
        b.ue(self.log2_frame_num - 4);
        match &self.poc {
            PocSpec::Type0 { log2_lsb } => {
                b.ue(0);
                b.ue(log2_lsb - 4);
            }
            PocSpec::Type1 {
                always_zero,
                non_ref,
                top_to_bottom,
                cycle,
            } => {
                b.ue(1);
                b.flag(*always_zero);
                b.se(*non_ref);
                b.se(*top_to_bottom);
                b.ue(cycle.len() as u32);
                for offset in cycle {
                    b.se(*offset);
                }
            }
            PocSpec::Type2 => b.ue(2),
        }
        b.ue(self.max_refs);
        b.flag(false);
        b.ue(self.width_mbs - 1);
        b.ue(self.height_map_units - 1);
        b.flag(self.frame_mbs_only);
        if !self.frame_mbs_only {
            b.flag(self.mbaff);
        }
        b.flag(true);
        b.flag(self.crop.is_some());
        if let Some(c) = self.crop {
            for v in c {
                b.ue(v);
            }
        }
        b.flag(false);
        rbsp(3, 7, b)
    }
}

/// A picture parameter set the test writes.
#[derive(Clone, Copy)]
struct PpsSpec {
    id: u32,
    sps_id: u32,
    cabac: bool,
    bottom_field_pic_order: bool,
    num_ref: [u32; 2],
    weighted_pred: bool,
    bipred_idc: u32,
    deblock_control: bool,
    transform_8x8: bool,
}

impl PpsSpec {
    fn plain(id: u32) -> Self {
        Self {
            id,
            sps_id: 0,
            cabac: false,
            bottom_field_pic_order: false,
            num_ref: [1, 1],
            weighted_pred: false,
            bipred_idc: 0,
            deblock_control: false,
            transform_8x8: false,
        }
    }

    fn rbsp(&self) -> Vec<u8> {
        let mut b = Bits::default();
        b.ue(self.id);
        b.ue(self.sps_id);
        b.flag(self.cabac);
        b.flag(self.bottom_field_pic_order);
        b.ue(0);
        b.ue(self.num_ref[0] - 1);
        b.ue(self.num_ref[1] - 1);
        b.flag(self.weighted_pred);
        b.u(2, self.bipred_idc);
        b.se(-4); // pic_init_qp_minus26
        b.se(0);
        b.se(-3); // chroma_qp_index_offset
        b.flag(self.deblock_control);
        b.flag(false);
        b.flag(false);
        if self.transform_8x8 {
            b.flag(true);
            b.flag(false);
            b.se(2); // second_chroma_qp_index_offset
        }
        rbsp(3, 8, b)
    }
}

/// A slice header the test writes, for a frame of `pps`.
#[derive(Clone)]
struct SliceSpec {
    unit_type: u8,
    ref_idc: u8,
    slice_type: u32,
    pps: PpsSpec,
    frame_num: u32,
    idr_id: u32,
    poc_lsb: u32,
    delta_poc_bottom: i32,
    delta_poc: [i32; 2],
    direct_spatial: bool,
    override_refs: Option<[u32; 2]>,
    /// The modifications of list 0 and 1 (`None` for no modification flag).
    rpl: [Option<Vec<(u32, u32)>>; 2],
    weights: bool,
    /// `memory_management_control_operation`s as (op, a, b); `None` for the sliding window.
    mmco: Option<Vec<(u32, u32, u32)>>,
    long_term_reference: bool,
    cabac_init_idc: u32,
    qp_delta: i32,
    deblock: Option<(u32, i32, i32)>,
}

impl SliceSpec {
    fn p(pps: PpsSpec, frame_num: u32) -> Self {
        Self {
            unit_type: 1,
            ref_idc: 2,
            slice_type: 5,
            pps,
            frame_num,
            idr_id: 0,
            poc_lsb: 0,
            delta_poc_bottom: 0,
            delta_poc: [0; 2],
            direct_spatial: false,
            override_refs: None,
            rpl: [None, None],
            weights: false,
            mmco: None,
            long_term_reference: false,
            cabac_init_idc: 0,
            qp_delta: 0,
            deblock: None,
        }
    }

    fn idr(pps: PpsSpec) -> Self {
        Self {
            unit_type: 5,
            ref_idc: 3,
            slice_type: 7,
            ..Self::p(pps, 0)
        }
    }

    fn rbsp(&self, sps: &SpsSpec) -> Vec<u8> {
        let mut b = Bits::default();
        b.ue(0);
        b.ue(self.slice_type);
        b.ue(self.pps.id);
        b.u(sps.log2_frame_num, self.frame_num);
        if !sps.frame_mbs_only {
            b.flag(false);
        }
        if self.unit_type == 5 {
            b.ue(self.idr_id);
        }
        match sps.poc {
            PocSpec::Type0 { log2_lsb } => {
                b.u(log2_lsb, self.poc_lsb);
                if self.pps.bottom_field_pic_order {
                    b.se(self.delta_poc_bottom);
                }
            }
            PocSpec::Type1 { always_zero, .. } if !always_zero => {
                b.se(self.delta_poc[0]);
                if self.pps.bottom_field_pic_order {
                    b.se(self.delta_poc[1]);
                }
            }
            _ => {}
        }
        let kind = self.slice_type % 5;
        if kind == 1 {
            b.flag(self.direct_spatial);
        }
        if kind == 0 || kind == 1 {
            b.flag(self.override_refs.is_some());
            if let Some([l0, l1]) = self.override_refs {
                b.ue(l0 - 1);
                if kind == 1 {
                    b.ue(l1 - 1);
                }
            }
            for list in 0..if kind == 1 { 2 } else { 1 } {
                b.flag(self.rpl[list].is_some());
                if let Some(mods) = &self.rpl[list] {
                    for &(idc, v) in mods {
                        b.ue(idc);
                        b.ue(v);
                    }
                    b.ue(3);
                }
            }
        }
        if self.weights {
            b.ue(5); // luma_log2_weight_denom
            b.ue(3); // chroma_log2_weight_denom
            let lists = if kind == 1 { 2 } else { 1 };
            let counts = self.override_refs.unwrap_or(self.pps.num_ref);
            for &count in counts.iter().take(lists) {
                for i in 0..count {
                    let luma = i % 2 == 0;
                    b.flag(luma);
                    if luma {
                        b.se(40 + i as i32);
                        b.se(-(i as i32));
                    }
                    b.flag(true);
                    for c in 0..2 {
                        b.se(9 + c);
                        b.se(-2 + c);
                    }
                }
            }
        }
        if self.ref_idc != 0 {
            if self.unit_type == 5 {
                b.flag(false);
                b.flag(self.long_term_reference);
            } else {
                b.flag(self.mmco.is_some());
                if let Some(ops) = &self.mmco {
                    for &(op, a, c) in ops {
                        b.ue(op);
                        match op {
                            1 | 2 => b.ue(a),
                            3 => {
                                b.ue(a);
                                b.ue(c);
                            }
                            4 | 6 => b.ue(c),
                            _ => {}
                        }
                    }
                    b.ue(0);
                }
            }
        }
        if self.pps.cabac && kind != 2 && kind != 4 {
            b.ue(self.cabac_init_idc);
        }
        b.se(self.qp_delta);
        if self.pps.deblock_control {
            let (idc, alpha, beta) = self.deblock.unwrap_or((0, 0, 0));
            b.ue(idc);
            if idc != 1 {
                b.se(alpha);
                b.se(beta);
            }
        }
        rbsp(self.ref_idc, self.unit_type, b)
    }
}

/// The parameter sets a test parses, with the SPS and PPS they name.
struct Ctx {
    sps: Sps,
    pps: Pps,
    sps_spec: SpsSpec,
}

impl Ctx {
    fn new(sps_spec: SpsSpec, pps_spec: PpsSpec) -> Self {
        let sps = parse_sps(&sps_spec.rbsp()).unwrap();
        let pps = parse_pps(&pps_spec.rbsp(), |_| Some(1)).unwrap();
        Self { sps, pps, sps_spec }
    }

    fn header(&self, spec: &SliceSpec) -> SliceHeader {
        let rbsp = spec.rbsp(&self.sps_spec);
        parse_slice_header(&rbsp, spec.unit_type, spec.ref_idc, &self.sps, &self.pps).unwrap()
    }
}

fn openh264_units() -> Vec<(u8, u8, Vec<u8>)> {
    units(OPENH264)
}

fn openh264_ctx() -> (Sps, Pps) {
    let mut sps = None;
    let mut pps = None;
    for (t, _, rbsp) in openh264_units() {
        match t {
            7 => sps = Some(parse_sps(&rbsp).unwrap()),
            8 => pps = Some(parse_pps(&rbsp, |_| Some(1)).unwrap()),
            _ => {}
        }
    }
    (sps.unwrap(), pps.unwrap())
}

/// The headers of the openh264 fixture's frames, in decode order.
fn openh264_headers() -> Vec<SliceHeader> {
    let (sps, pps) = openh264_ctx();
    openh264_units()
        .into_iter()
        .filter(|(t, _, _)| *t == 1 || *t == 5)
        .map(|(t, r, rbsp)| parse_slice_header(&rbsp, t, r, &sps, &pps).unwrap())
        .collect()
}

#[test]
fn annexb_splits_three_and_four_byte_start_codes_and_drops_trailing_zeros() {
    let stream = [
        0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 9, 0, 0,
    ];
    let got: Vec<(u8, u8, Vec<u8>)> = nal_units(&stream)
        .map(|n| (n.unit_type, n.nal_ref_idc, n.raw.to_vec()))
        .collect();
    assert_eq!(
        got,
        vec![
            (7, 3, vec![0x67, 1, 2]),
            (8, 3, vec![0x68, 3]),
            (5, 3, vec![0x65, 9]),
        ]
    );
}

#[test]
fn a_forbidden_bit_is_refused_by_the_parsers() {
    let mut sps = SpsSpec::baseline(1, 2, 2).rbsp();
    sps[0] |= 0x80;
    assert!(matches!(parse_sps(&sps), Err(Error::Invalid(_))));
}

#[test]
fn the_openh264_sps_is_the_encoders_settings() {
    let (sps, _) = openh264_ctx();
    assert_eq!(
        (sps.profile_idc, sps.constraint_flags, sps.level_idc),
        (66, 0xc0, 20)
    );
    assert_eq!(
        (sps.id, sps.chroma_format_idc, sps.bit_depth_luma_minus8),
        (0, 1, 0)
    );
    assert_eq!((sps.width_mbs, sps.height_map_units), (6, 4));
    assert_eq!(sps.frame_size(), (96, 64));
    assert!(sps.frame_mbs_only && sps.crop.is_none() && sps.scaling.is_none());
    assert_eq!(sps.poc_type, 2);
    assert_eq!(sps.max_num_ref_frames, 1);
    assert!(!sps.gaps_allowed);
}

#[test]
fn the_openh264_pps_is_the_encoders_settings() {
    let (_, pps) = openh264_ctx();
    assert_eq!((pps.id, pps.sps_id), (0, 0));
    assert!(!pps.cabac && !pps.bottom_field_pic_order_present);
    assert_eq!(pps.num_ref_idx_default, [1, 1]);
    assert_eq!((pps.pic_init_qp, pps.pic_init_qs), (26, 26));
    assert!(pps.deblocking_filter_control_present);
    assert!(!pps.transform_8x8_mode && pps.scaling.is_none());
    assert_eq!(
        pps.second_chroma_qp_index_offset,
        pps.chroma_qp_index_offset
    );
}

#[test]
fn the_openh264_slices_follow_the_gop() {
    let headers = openh264_headers();
    let types: Vec<u8> = headers.iter().map(|h| h.nal_unit_type).collect();
    assert_eq!(types, [5, 1, 1, 1, 5, 1, 1, 1, 5]);
    let frame_nums: Vec<u32> = headers.iter().map(|h| h.frame_num).collect();
    assert_eq!(frame_nums, [0, 1, 2, 3, 0, 1, 2, 3, 0]);
    let kinds: Vec<SliceKind> = headers.iter().map(|h| h.kind).collect();
    assert!(kinds.iter().enumerate().all(|(i, k)| *k
        == if i % 4 == 0 {
            SliceKind::I
        } else {
            SliceKind::P
        }));
    let ids: Vec<u32> = headers
        .iter()
        .filter(|h| h.is_idr())
        .map(|h| h.idr_pic_id)
        .collect();
    assert_eq!(ids, [1, 2, 3]);
    for h in &headers {
        assert!(h.first_mb_in_slice == 0 && h.nal_ref_idc == 3);
        assert!(h.data_bit_offset > 0 && h.data_bit_offset < 8 * 1300);
    }
    // P slices take one reference, and the openh264 encoder writes one list modification.
    for h in headers.iter().filter(|h| h.kind == SliceKind::P) {
        assert_eq!(h.num_ref_idx_active, [1, 0]);
        assert!(h.rpl_modified[0]);
        assert_eq!(h.modifications(0), [RplMod { idc: 0, value: 0 }]);
    }
}

#[test]
fn the_openh264_order_counts_are_type_2() {
    let (sps, _) = openh264_ctx();
    let mut state = PocState::default();
    let mut counts = Vec::new();
    for (h, (t, _, _)) in openh264_headers()
        .iter()
        .zip(openh264_units().iter().filter(|u| u.0 == 1 || u.0 == 5))
    {
        assert!(*t == 1 || *t == 5);
        let pic = state.compute(h, &sps).unwrap();
        state.commit(h, &pic);
        counts.push(pic.poc.frame());
    }
    // An IDR restarts the count at 0; a P frame's count is 2 * FrameNumOffset + frame_num.
    assert_eq!(counts, [0, 2, 4, 6, 0, 2, 4, 6, 0]);
}

#[test]
fn the_openh264_reference_lists_name_the_previous_frame() {
    let (sps, _) = openh264_ctx();
    let mut dpb = Dpb::new();
    let mut state = PocState::default();
    for (index, h) in openh264_headers().iter().enumerate() {
        dpb.check_frame_num(h, &sps).unwrap();
        let pic = state.compute(h, &sps).unwrap();
        let lists = dpb.lists(h, &sps, pic.poc).unwrap();
        if h.kind == SliceKind::I {
            assert_eq!(lists.list(0).len(), 0);
        } else {
            let entry = lists.list(0)[0];
            let frame = dpb
                .get(usize::from(entry))
                .expect("the list names a DPB frame");
            // With one reference frame, the previous picture is the only candidate.
            assert_eq!(frame.stamp_ns as usize, index - 1);
            assert_eq!(frame.frame_num, h.frame_num - 1);
        }
        state.commit(h, &pic);
        dpb.mark(h, &sps, index as u64, pic.stored());
    }
    assert_eq!(dpb.frames().count(), 1);
}

#[test]
fn a_truncated_or_flipped_header_is_an_error_never_a_panic() {
    let units = openh264_units();
    let (sps, pps) = openh264_ctx();
    for (t, r, rbsp) in units.iter().take(9) {
        for len in 0..rbsp.len().min(64) {
            let cut = &rbsp[..len];
            let _ = match *t {
                7 => parse_sps(cut).map(|_| ()),
                8 => parse_pps(cut, |_| Some(1)).map(|_| ()),
                _ => parse_slice_header(cut, *t, *r, &sps, &pps).map(|_| ()),
            };
        }
        for bit in 0..(rbsp.len().min(48) * 8) {
            let mut flipped = rbsp.clone();
            flipped[bit / 8] ^= 0x80 >> (bit % 8);
            let _ = match *t {
                7 => parse_sps(&flipped).map(|_| ()),
                8 => parse_pps(&flipped, |_| Some(1)).map(|_| ()),
                _ => parse_slice_header(&flipped, *t, *r, &sps, &pps).map(|_| ()),
            };
        }
    }
}

#[test]
fn a_b_slice_header_round_trips_every_field() {
    let sps_spec = SpsSpec {
        profile: 100,
        chroma: 1,
        log2_frame_num: 8,
        poc: PocSpec::Type0 { log2_lsb: 8 },
        max_refs: 4,
        width_mbs: 4,
        height_map_units: 3,
        frame_mbs_only: false,
        mbaff: true,
        crop: Some([0, 0, 0, 1]),
    };
    let pps_spec = PpsSpec {
        id: 1,
        sps_id: 0,
        cabac: true,
        bottom_field_pic_order: true,
        num_ref: [2, 3],
        weighted_pred: true,
        bipred_idc: 1,
        deblock_control: true,
        transform_8x8: true,
    };
    // The SPS with a scaling matrix, whose list 0 is flat 8 and whose list 2 takes the default.
    assert_eq!(
        parse_sps(&sps_spec.rbsp()).unwrap().crop,
        Some([0, 0, 0, 1])
    );
    let mut b = Bits::default();
    b.u(8, 100);
    b.u(8, 0);
    b.u(8, 30);
    b.ue(0);
    b.ue(1);
    b.ue(0);
    b.ue(0);
    b.flag(false);
    b.flag(true); // seq_scaling_matrix_present_flag
    for i in 0..8 {
        match i {
            0 => {
                b.flag(true);
                for _ in 0..16 {
                    b.se(0);
                }
            }
            2 => {
                b.flag(true);
                b.se(-8); // nextScale 0 at j 0: the default matrix
            }
            _ => b.flag(false),
        }
    }
    b.ue(4); // log2_max_frame_num_minus4: 8 bits
    b.ue(0); // pic_order_cnt_type 0
    b.ue(4); // log2_max_pic_order_cnt_lsb_minus4: 8 bits
    b.ue(4); // max_num_ref_frames
    b.flag(false); // gaps_in_frame_num_value_allowed_flag
    b.ue(3); // 4 macroblocks wide
    b.ue(2); // 3 map units high: 6 macroblock rows as a frame
    b.flag(false); // frame_mbs_only_flag
    b.flag(true); // mb_adaptive_frame_field_flag
    b.flag(true); // direct_8x8_inference_flag
    b.flag(false); // frame_cropping_flag
    b.flag(false); // vui_parameters_present_flag
    let sps = parse_sps(&rbsp(3, 7, b)).unwrap();
    let scaling = sps.scaling.as_ref().expect("the SPS has a scaling matrix");
    assert!(scaling.present[0] && scaling.present[2] && !scaling.present[1]);
    assert_eq!(scaling.values[0][..16], [8; 16]);
    assert!(scaling.use_default[2] && !scaling.use_default[0]);
    assert_eq!(sps.log2_max_frame_num, 8);
    assert_eq!(sps.poc_type, 0);
    assert_eq!(sps.log2_max_poc_lsb, 8);
    assert!(sps.mb_adaptive_frame_field && !sps.frame_mbs_only);
    assert_eq!(sps.frame_size(), (64, 96));

    let pps_rbsp = {
        let mut b = Bits::default();
        b.ue(1);
        b.ue(0);
        b.flag(true);
        b.flag(true);
        b.ue(0);
        b.ue(1);
        b.ue(2);
        b.flag(true);
        b.u(2, 1);
        b.se(-4);
        b.se(0);
        b.se(-3);
        b.flag(true);
        b.flag(false);
        b.flag(false);
        b.flag(true); // transform_8x8_mode_flag
        b.flag(true); // pic_scaling_matrix_present_flag
        for i in 0..8 {
            match i {
                0 => {
                    b.flag(true);
                    for _ in 0..16 {
                        b.se(0);
                    }
                }
                _ => b.flag(false),
            }
        }
        b.se(2);
        rbsp(3, 8, b)
    };
    let pps = parse_pps(&pps_rbsp, |id| (id == 0).then_some(1)).unwrap();
    assert_eq!((pps.id, pps.sps_id, pps.cabac), (1, 0, true));
    assert_eq!(pps.num_ref_idx_default, [2, 3]);
    assert_eq!((pps.weighted_pred, pps.weighted_bipred_idc), (true, 1));
    assert_eq!((pps.pic_init_qp, pps.chroma_qp_index_offset), (22, -3));
    assert!(pps.transform_8x8_mode && pps.scaling.is_some());
    assert_eq!(pps.second_chroma_qp_index_offset, 2);

    let ctx = Ctx {
        sps,
        pps,
        sps_spec: sps_spec.clone(),
    };
    let mut spec = SliceSpec {
        unit_type: 1,
        ref_idc: 0,
        slice_type: 6,
        pps: pps_spec,
        frame_num: 5,
        idr_id: 0,
        poc_lsb: 100,
        delta_poc_bottom: -1,
        delta_poc: [0; 2],
        direct_spatial: true,
        override_refs: Some([3, 2]),
        rpl: [Some(vec![(0, 0), (2, 1)]), None],
        weights: true,
        mmco: None,
        long_term_reference: false,
        cabac_init_idc: 2,
        qp_delta: -3,
        deblock: Some((0, 2, -1)),
    };
    let h = ctx.header(&spec);
    assert_eq!(
        (h.kind, h.pps_id, h.frame_num, h.poc_lsb),
        (SliceKind::B, 1, 5, 100)
    );
    assert_eq!((h.delta_poc_bottom, h.direct_spatial_mv_pred), (-1, true));
    assert_eq!(h.num_ref_idx_active, [3, 2]);
    assert_eq!(
        h.modifications(0),
        [RplMod { idc: 0, value: 0 }, RplMod { idc: 2, value: 1 }]
    );
    assert!(h.rpl_modified == [true, false] && h.rpl_count == [2, 0]);
    let w = h.weights.as_ref().expect("the weight table is present");
    assert_eq!((w.luma_log2_denom, w.chroma_log2_denom), (5, 3));
    assert_eq!(w.entries[0][0].luma, Some((40, 0)));
    assert_eq!(w.entries[0][1].luma, None);
    assert_eq!(w.entries[0][2].luma, Some((42, -2)));
    assert_eq!(w.entries[0][0].chroma, Some([(9, -2), (10, -1)]));
    assert_eq!(w.entries[1][1].chroma, Some([(9, -2), (10, -1)]));
    assert!(h.mmcos().is_empty() && !h.adaptive_marking);
    assert_eq!((h.cabac_init_idc, h.qp_delta), (2, -3));
    assert_eq!(
        (
            h.disable_deblocking_filter_idc,
            h.alpha_c0_offset_div2,
            h.beta_offset_div2
        ),
        (0, 2, -1)
    );
    assert!(h.data_bit_offset <= 8 * 64);

    // A reference B slice with memory management operations and the long-term marking.
    spec.ref_idc = 2;
    spec.mmco = Some(vec![(3, 1, 2), (4, 0, 3), (6, 0, 1)]);
    spec.unit_type = 1;
    let h = ctx.header(&spec);
    assert_eq!(
        h.mmcos(),
        [
            Mmco { op: 3, a: 1, b: 2 },
            Mmco { op: 4, a: 0, b: 3 },
            Mmco { op: 6, a: 0, b: 1 }
        ]
    );
    assert!(h.adaptive_marking);
}

#[test]
fn an_idr_slice_marking_round_trips() {
    let ctx = Ctx::new(SpsSpec::baseline(4, 2, 2), PpsSpec::plain(0));
    let mut spec = SliceSpec::idr(PpsSpec::plain(0));
    spec.long_term_reference = true;
    spec.idr_id = 7;
    let h = ctx.header(&spec);
    assert!(h.is_idr() && h.long_term_reference && !h.no_output_of_prior_pics);
    assert_eq!(h.idr_pic_id, 7);
    assert_eq!(h.kind, SliceKind::I);
    assert_eq!(h.num_ref_idx_active, [0, 0]);
    assert_eq!(h.qp_delta, 0);
}

#[test]
fn an_idr_with_reference_idc_zero_is_refused() {
    let ctx = Ctx::new(SpsSpec::baseline(1, 2, 2), PpsSpec::plain(0));
    let mut spec = SliceSpec::idr(PpsSpec::plain(0));
    spec.ref_idc = 0;
    let rbsp = spec.rbsp(&ctx.sps_spec);
    assert!(matches!(
        parse_slice_header(&rbsp, 5, 0, &ctx.sps, &ctx.pps),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn field_pictures_and_gaps_are_refused_as_unsupported() {
    let mut sps_spec = SpsSpec::baseline(2, 2, 2);
    sps_spec.frame_mbs_only = false;
    let ctx = Ctx::new(sps_spec, PpsSpec::plain(0));
    let mut b = Bits::default();
    b.ue(0);
    b.ue(5);
    b.ue(0);
    b.u(4, 1);
    b.flag(true); // field_pic_flag
    b.flag(false);
    let rbsp = rbsp(2, 1, b);
    assert!(matches!(
        parse_slice_header(&rbsp, 1, 2, &ctx.sps, &ctx.pps),
        Err(Error::Unsupported(_))
    ));

    let mut dpb = Dpb::new();
    let idr = ctx.header(&SliceSpec::idr(PpsSpec::plain(0)));
    dpb.mark(&idr, &ctx.sps, 0, Poc::default());
    let mut gap = SliceSpec::p(PpsSpec::plain(0), 2);
    gap.ref_idc = 2;
    let h = ctx.header(&gap);
    assert!(matches!(
        dpb.check_frame_num(&h, &ctx.sps),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn a_fmo_pps_is_refused() {
    let mut b = Bits::default();
    b.ue(0);
    b.ue(0);
    b.flag(false);
    b.flag(false);
    b.ue(1); // num_slice_groups_minus1
    let rbsp = rbsp(3, 8, b);
    assert!(matches!(
        parse_pps(&rbsp, |_| Some(1)),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn poc_type_0_wraps_the_lsb_into_the_msb() {
    let mut sps_spec = SpsSpec::baseline(1, 2, 2);
    sps_spec.poc = PocSpec::Type0 { log2_lsb: 4 };
    let ctx = Ctx::new(sps_spec, PpsSpec::plain(0));
    let mut state = PocState::default();
    let mut tops = Vec::new();
    let idr = ctx.header(&SliceSpec::idr(PpsSpec::plain(0)));
    let pic = state.compute(&idr, &ctx.sps).unwrap();
    state.commit(&idr, &pic);
    tops.push(pic.poc.top);
    for (fnum, lsb) in [(1, 14), (2, 2)] {
        let mut s = SliceSpec::p(PpsSpec::plain(0), fnum);
        s.poc_lsb = lsb;
        let h = ctx.header(&s);
        let pic = state.compute(&h, &ctx.sps).unwrap();
        state.commit(&h, &pic);
        tops.push(pic.poc.top);
    }
    // 14 is 14 below 0 wrapped: msb -16, so -2; then 2 comes after 14 with msb 0.
    assert_eq!(tops, [0, -2, 2]);
}

#[test]
fn poc_type_1_follows_the_offset_cycle_and_the_non_reference_offset() {
    let mut sps_spec = SpsSpec::baseline(4, 2, 2);
    sps_spec.poc = PocSpec::Type1 {
        always_zero: false,
        non_ref: -1,
        top_to_bottom: 1,
        cycle: vec![4, 2],
    };
    sps_spec.log2_frame_num = 4;
    let mut pps = PpsSpec::plain(0);
    pps.bottom_field_pic_order = true;
    let ctx = Ctx::new(sps_spec, pps);
    let mut state = PocState::default();
    let mut counts = Vec::new();
    // Reference frames 0, 1, 2, then a non-reference picture with frame_num 3.
    let specs = [
        SliceSpec::idr(pps),
        SliceSpec::p(pps, 1),
        SliceSpec::p(pps, 2),
        {
            let mut s = SliceSpec::p(pps, 3);
            s.ref_idc = 0;
            s
        },
    ];
    for spec in &specs {
        let h = ctx.header(spec);
        let pic = state.compute(&h, &ctx.sps).unwrap();
        state.commit(&h, &pic);
        counts.push((pic.poc.top, pic.poc.bottom));
    }
    assert_eq!(counts, [(0, 1), (4, 5), (6, 7), (5, 6)]);
}

#[test]
fn poc_type_2_counts_two_per_frame_and_one_less_for_non_reference() {
    let mut sps_spec = SpsSpec::baseline(2, 2, 2);
    sps_spec.poc = PocSpec::Type2;
    let ctx = Ctx::new(sps_spec, PpsSpec::plain(0));
    let mut state = PocState::default();
    let mut tops = Vec::new();
    let idr = ctx.header(&SliceSpec::idr(PpsSpec::plain(0)));
    let mut s1 = SliceSpec::p(PpsSpec::plain(0), 1);
    s1.ref_idc = 2;
    let mut s2 = SliceSpec::p(PpsSpec::plain(0), 2);
    s2.ref_idc = 0;
    for h in [idr, ctx.header(&s1), ctx.header(&s2)] {
        let pic = state.compute(&h, &ctx.sps).unwrap();
        state.commit(&h, &pic);
        tops.push(pic.poc.top);
    }
    assert_eq!(tops, [0, 2, 3]);
}

#[test]
fn a_mmco5_restarts_the_count_after_its_picture() {
    let mut sps_spec = SpsSpec::baseline(2, 2, 2);
    sps_spec.poc = PocSpec::Type2;
    let ctx = Ctx::new(sps_spec, PpsSpec::plain(0));
    let mut state = PocState::default();
    let idr = ctx.header(&SliceSpec::idr(PpsSpec::plain(0)));
    let pic = state.compute(&idr, &ctx.sps).unwrap();
    state.commit(&idr, &pic);
    let mut s = SliceSpec::p(PpsSpec::plain(0), 1);
    s.mmco = Some(vec![(5, 0, 0)]);
    let h = ctx.header(&s);
    let pic = state.compute(&h, &ctx.sps).unwrap();
    assert!(pic.mmco5);
    assert_eq!(pic.poc.top, 2);
    assert_eq!(pic.stored().top, 0);
    state.commit(&h, &pic);
    let next = ctx.header(&SliceSpec::p(PpsSpec::plain(0), 1));
    let pic = state.compute(&next, &ctx.sps).unwrap();
    assert_eq!(pic.poc.top, 2);
}

#[test]
fn a_b_frame_lists_order_by_order_count_and_list_1_starts_with_the_later_frame() {
    let mut sps_spec = SpsSpec::baseline(2, 2, 2);
    sps_spec.poc = PocSpec::Type0 { log2_lsb: 8 };
    let pps = PpsSpec::plain(0);
    let ctx = Ctx::new(sps_spec, pps);
    let mut dpb = Dpb::new();
    let mut state = PocState::default();
    // I (poc 0, frame 0), P (poc 8, frame 1), then a B picture (poc 4, frame 2, not a reference).
    for (i, mut spec) in [SliceSpec::idr(pps), SliceSpec::p(pps, 1)]
        .into_iter()
        .enumerate()
    {
        spec.poc_lsb = if i == 0 { 0 } else { 8 };
        let h = ctx.header(&spec);
        let pic = state.compute(&h, &ctx.sps).unwrap();
        state.commit(&h, &pic);
        dpb.mark(&h, &ctx.sps, i as u64, pic.stored());
    }
    let mut b = SliceSpec::p(pps, 2);
    b.slice_type = 6;
    b.ref_idc = 0;
    b.poc_lsb = 4;
    b.override_refs = Some([2, 2]);
    let h = ctx.header(&b);
    let pic = state.compute(&h, &ctx.sps).unwrap();
    let lists = dpb.lists(&h, &ctx.sps, pic.poc).unwrap();
    let buffer = |list: &[u8]| -> Vec<u64> {
        list.iter()
            .map(|&s| dpb.get(usize::from(s)).unwrap().stamp_ns)
            .collect()
    };
    assert_eq!(buffer(lists.list(0)), [0, 1]);
    assert_eq!(buffer(lists.list(1)), [1, 0]);
}

#[test]
fn list_1_identical_to_list_0_has_its_first_two_switched() {
    let mut sps_spec = SpsSpec::baseline(2, 2, 2);
    sps_spec.poc = PocSpec::Type0 { log2_lsb: 8 };
    let pps = PpsSpec::plain(0);
    let ctx = Ctx::new(sps_spec, pps);
    let mut dpb = Dpb::new();
    let mut state = PocState::default();
    // Two reference frames, both before the B picture: list 0 is [poc 2, poc 0] and so is the
    // list 1 of the frames before it, so the switch gives list 1 [poc 0, poc 2].
    for (i, lsb) in [(0u32, 0u32), (1, 2)] {
        let mut spec = if i == 0 {
            SliceSpec::idr(pps)
        } else {
            SliceSpec::p(pps, 1)
        };
        spec.poc_lsb = lsb;
        let h = ctx.header(&spec);
        let pic = state.compute(&h, &ctx.sps).unwrap();
        state.commit(&h, &pic);
        dpb.mark(&h, &ctx.sps, u64::from(i), pic.stored());
    }
    let mut b = SliceSpec::p(pps, 2);
    b.slice_type = 6;
    b.ref_idc = 0;
    b.poc_lsb = 4;
    b.override_refs = Some([2, 2]);
    let h = ctx.header(&b);
    let pic = state.compute(&h, &ctx.sps).unwrap();
    let lists = dpb.lists(&h, &ctx.sps, pic.poc).unwrap();
    let frame = |list: &[u8]| -> Vec<u64> {
        list.iter()
            .map(|&s| dpb.get(usize::from(s)).unwrap().stamp_ns)
            .collect()
    };
    assert_eq!(frame(lists.list(0)), [1, 0]);
    assert_eq!(frame(lists.list(1)), [0, 1]);
}

#[test]
fn a_list_modification_moves_the_named_frame_to_the_front() {
    let ctx = Ctx::new(SpsSpec::baseline(3, 2, 2), PpsSpec::plain(0));
    let mut dpb = Dpb::new();
    let mut state = PocState::default();
    let idr = ctx.header(&SliceSpec::idr(PpsSpec::plain(0)));
    let pic = state.compute(&idr, &ctx.sps).unwrap();
    state.commit(&idr, &pic);
    dpb.mark(&idr, &ctx.sps, 0, pic.stored());
    for fnum in 1..=2 {
        let mut s = SliceSpec::p(PpsSpec::plain(0), fnum);
        s.ref_idc = 2;
        let h = ctx.header(&s);
        let pic = state.compute(&h, &ctx.sps).unwrap();
        state.commit(&h, &pic);
        dpb.mark(&h, &ctx.sps, u64::from(fnum), pic.stored());
    }
    // The current frame is 3. Abs diff minus one 2 gives picture number 0: the IDR, which the
    // list would otherwise have last.
    let mut s = SliceSpec::p(PpsSpec::plain(0), 3);
    s.override_refs = Some([3, 1]);
    s.rpl[0] = Some(vec![(0, 2)]);
    let h = ctx.header(&s);
    let pic = state.compute(&h, &ctx.sps).unwrap();
    let lists = dpb.lists(&h, &ctx.sps, pic.poc).unwrap();
    let frame_nums: Vec<u32> = lists
        .list(0)
        .iter()
        .map(|&s| dpb.get(usize::from(s)).unwrap().frame_num)
        .collect();
    assert_eq!(frame_nums, [0, 2, 1]);
}

#[test]
fn a_modification_of_a_missing_frame_is_refused() {
    let ctx = Ctx::new(SpsSpec::baseline(2, 2, 2), PpsSpec::plain(0));
    let mut dpb = Dpb::new();
    let idr = ctx.header(&SliceSpec::idr(PpsSpec::plain(0)));
    dpb.mark(&idr, &ctx.sps, 0, Poc::default());
    let mut s = SliceSpec::p(PpsSpec::plain(0), 1);
    s.rpl[0] = Some(vec![(0, 4)]);
    let h = ctx.header(&s);
    assert!(matches!(
        dpb.lists(&h, &ctx.sps, Poc::default()),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn the_sliding_window_keeps_the_newest_max_num_ref_frames() {
    let ctx = Ctx::new(SpsSpec::baseline(2, 2, 2), PpsSpec::plain(0));
    let mut dpb = Dpb::new();
    let idr = ctx.header(&SliceSpec::idr(PpsSpec::plain(0)));
    dpb.mark(&idr, &ctx.sps, 0, Poc::default());
    for fnum in 1..=3 {
        let mut s = SliceSpec::p(PpsSpec::plain(0), fnum);
        s.ref_idc = 2;
        let h = ctx.header(&s);
        dpb.mark(&h, &ctx.sps, u64::from(fnum), Poc::default());
    }
    let mut kept: Vec<u32> = dpb.frames().map(|(_, f)| f.frame_num).collect();
    kept.sort_unstable();
    assert_eq!(kept, [2, 3]);
}

#[test]
fn mmco_1_unmarks_and_mmco_6_marks_the_current_frame_long_term() {
    let ctx = Ctx::new(SpsSpec::baseline(3, 2, 2), PpsSpec::plain(0));
    let mut dpb = Dpb::new();
    let idr = ctx.header(&SliceSpec::idr(PpsSpec::plain(0)));
    dpb.mark(&idr, &ctx.sps, 0, Poc::default());
    // Frame 1 becomes long-term index 0 (mmco 6).
    let mut s1 = SliceSpec::p(PpsSpec::plain(0), 1);
    s1.ref_idc = 2;
    s1.mmco = Some(vec![(6, 0, 0)]);
    let h1 = ctx.header(&s1);
    dpb.mark(&h1, &ctx.sps, 1, Poc::default());
    // Frame 2 with the sliding window keeps the IDR, and the list puts the long-term frame last.
    let mut s2 = SliceSpec::p(PpsSpec::plain(0), 2);
    s2.ref_idc = 2;
    s2.override_refs = Some([2, 1]);
    let h2 = ctx.header(&s2);
    let lists = dpb.lists(&h2, &ctx.sps, Poc::default()).unwrap();
    let frames: Vec<(u32, Option<u32>)> = lists
        .list(0)
        .iter()
        .map(|&s| {
            let f = dpb.get(usize::from(s)).unwrap();
            (f.frame_num, f.long_term)
        })
        .collect();
    assert_eq!(frames, [(0, None), (1, Some(0))]);
    dpb.mark(&h2, &ctx.sps, 2, Poc::default());
    assert_eq!(dpb.max_long_term_idx(), None);

    // Frame 3 with mmco 1 (difference minus one 0: picture number 3 - 1 = 2, the short-term
    // frame 2 goes; the long-term frame 1 is not a short-term picture number).
    let mut s3 = SliceSpec::p(PpsSpec::plain(0), 3);
    s3.ref_idc = 2;
    s3.mmco = Some(vec![(1, 0, 0)]);
    let h3 = ctx.header(&s3);
    dpb.mark(&h3, &ctx.sps, 3, Poc::default());
    let kept: Vec<(u32, Option<u32>)> = dpb
        .frames()
        .map(|(_, f)| (f.frame_num, f.long_term))
        .collect();
    assert!(kept.contains(&(1, Some(0))) && kept.contains(&(3, None)));
    assert!(!kept.iter().any(|k| k.0 == 2));
}

#[test]
fn a_non_reference_picture_is_not_kept() {
    let ctx = Ctx::new(SpsSpec::baseline(2, 2, 2), PpsSpec::plain(0));
    let mut dpb = Dpb::new();
    let mut s = SliceSpec::p(PpsSpec::plain(0), 0);
    s.ref_idc = 0;
    let h = ctx.header(&s);
    dpb.mark(&h, &ctx.sps, 0, Poc::default());
    assert_eq!(dpb.frames().count(), 0);
}

#[test]
fn the_nal_splitter_reads_the_header_bits() {
    let stream = annexb(&rbsp(2, 1, Bits::default()));
    let nal = nal_units(&stream).next().unwrap();
    assert_eq!((nal.nal_ref_idc, nal.unit_type), (2, 1));
    assert_eq!(nal.raw[0], (2 << 5) | 1);
}

#[test]
fn styx_h264s_own_stream_parses_to_its_settings() {
    use styx_h264::{StyxH264, StyxH264Input};
    let (width, height) = (48usize, 32usize);
    let mut encoder = StyxH264::new(28, 3);
    let y = vec![96u8; width * height];
    let mut stream = Vec::new();
    let mut idrs = Vec::new();
    for _ in 0..5 {
        let mut out = Vec::new();
        let input = StyxH264Input::Gray {
            y: &y,
            width,
            height,
            stride: width,
        };
        idrs.push(encoder.encode(&input, &mut out).unwrap());
        stream.extend_from_slice(&out);
    }
    let units = units(&stream);
    let sps = parse_sps(&units.iter().find(|u| u.0 == 7).unwrap().2).unwrap();
    assert_eq!(sps.frame_size(), (48, 32));
    assert_eq!(
        (sps.profile_idc, sps.poc_type, sps.max_num_ref_frames),
        (66, 2, 1)
    );
    let pps = parse_pps(&units.iter().find(|u| u.0 == 8).unwrap().2, |_| Some(1)).unwrap();
    assert!(!pps.cabac && pps.num_ref_idx_default == [1, 1]);
    let headers: Vec<SliceHeader> = units
        .iter()
        .filter(|u| u.0 == 1 || u.0 == 5)
        .map(|(t, r, rbsp)| parse_slice_header(rbsp, *t, *r, &sps, &pps).unwrap())
        .collect();
    let idr_frames: Vec<bool> = headers.iter().map(|h| h.is_idr()).collect();
    assert_eq!(idr_frames, idrs);
    assert_eq!(idrs, [true, false, false, true, false]);
    let frame_nums: Vec<u32> = headers.iter().map(|h| h.frame_num).collect();
    assert_eq!(frame_nums, [0, 1, 2, 0, 1]);
}
