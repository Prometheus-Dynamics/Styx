//! Parser tests: bit reading and NAL splitting on constructed bytes, the
//! parameter sets and an IDR slice header of a stream encoded on a host (one
//! 320x240 IDR frame from a hardware encoder), and that stream's headers
//! truncated and corrupted.

use super::*;
use std::vec::Vec;

/// The fixture: an AUD, VPS, SPS, PPS and one IDR_W_RADL slice, Annex B.
const IDR_STREAM: &[u8] = include_bytes!("fixtures/idr320.h265");

/// Bits from a string of `0` and `1` (spaces ignored), packed MSB first.
fn pack(bits: &str) -> Vec<u8> {
    let bits: Vec<u8> = bits.bytes().filter(|b| *b == b'0' || *b == b'1').collect();
    bits.chunks(8)
        .map(|c| {
            let mut byte = 0u8;
            for (i, b) in c.iter().enumerate() {
                byte |= (b - b'0') << (7 - i);
            }
            byte
        })
        .collect()
}

#[test]
fn the_fixture_splits_into_its_nal_units() {
    let types: Vec<u8> = nal_units(IDR_STREAM).map(|n| n.unit_type).collect();
    assert_eq!(types, [35, 32, 33, 34, 19]);
    let slice = nal_units(IDR_STREAM).last().unwrap();
    assert_eq!(slice.layer_id, 0);
    assert_eq!(slice.temporal_id_plus1, 1);
    // The unit's bytes end where the stream does (no trailing zeros are kept).
    assert_eq!(slice.raw.last(), IDR_STREAM.last());
}

#[test]
fn exp_golomb_codes_decode() {
    let data = pack("1 010 011 00100 00101 0001000");
    let mut r = BitReader::new(&data);
    assert_eq!(r.ue().unwrap(), 0);
    assert_eq!(r.ue().unwrap(), 1);
    assert_eq!(r.ue().unwrap(), 2);
    assert_eq!(r.ue().unwrap(), 3);
    assert_eq!(r.ue().unwrap(), 4);
    assert_eq!(r.ue().unwrap(), 7);

    let data = pack("010 011 00100 00101");
    let mut r = BitReader::new(&data);
    assert_eq!(r.se().unwrap(), 1);
    assert_eq!(r.se().unwrap(), -1);
    assert_eq!(r.se().unwrap(), 2);
    assert_eq!(r.se().unwrap(), -2);
}

#[test]
fn reads_past_the_end_are_truncated() {
    let data = pack("1");
    let mut r = BitReader::new(&data);
    assert_eq!(r.bits(9), Err(Error::Truncated));
    let data = [0u8; 2];
    assert_eq!(BitReader::new(&data).ue(), Err(Error::Truncated));
}

#[test]
fn emulation_prevention_bytes_are_removed_and_indexed() {
    let raw = [0x26, 0x01, 0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x00];
    let mut out = [0u8; 16];
    let n = unescape(&raw, &mut out).unwrap();
    assert_eq!(&out[..n], [0x26, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]);
    // RBSP byte 4 (the 0x01 after the escape) is raw byte 5.
    assert_eq!(raw_index(&raw, 4), Some(5));
    assert_eq!(raw_index(&raw, 0), Some(0));
    assert_eq!(raw_index(&raw, 99), None);
    assert_eq!(unescape(&raw, &mut out[..3]), Err(Error::Buffer));
}

#[test]
fn a_four_byte_start_code_does_not_leak_a_zero_into_the_unit() {
    let stream = [
        0, 0, 0, 1, 0x42, 0x01, 0xAA, 0, 0, 0, 1, 0x44, 0x01, 0xBB, 0, 0,
    ];
    let units: Vec<&[u8]> = nal_units(&stream).map(|n| n.raw).collect();
    assert_eq!(units, [&[0x42, 0x01, 0xAA][..], &[0x44, 0x01, 0xBB][..]]);
}

#[test]
fn the_vps_gives_its_identifier_and_sub_layers() {
    let vps = nal_units(IDR_STREAM).find(|n| n.unit_type == 32).unwrap();
    let mut buf = [0u8; 64];
    let n = unescape(vps.raw, &mut buf).unwrap();
    let v = parse_vps(&buf[..n]).unwrap();
    assert_eq!(v.id, 0);
    assert_eq!(v.max_layers_minus1, 0);
    assert_eq!(v.max_sub_layers_minus1, 0);
}

#[test]
fn the_sps_of_the_host_stream_gives_its_size_and_tools() {
    let sps = nal_units(IDR_STREAM).find(|n| n.unit_type == 33).unwrap();
    let mut buf = [0u8; 128];
    let n = unescape(sps.raw, &mut buf).unwrap();
    let s = parse_sps(&buf[..n]).unwrap();
    assert_eq!((s.width, s.height), (320, 240));
    assert_eq!(s.id, 0);
    assert_eq!(s.chroma_format_idc, 1);
    assert_eq!(s.chroma_array_type(), 1);
    assert_eq!((s.bit_depth_luma_minus8, s.bit_depth_chroma_minus8), (0, 0));
    assert_eq!(s.log2_ctb(), 6);
    assert_eq!(s.ctbs(), Some((5, 4)));
    assert!(!s.scaling_list_enabled);
}

#[test]
fn the_pps_of_the_host_stream_is_read_to_its_end() {
    let pps = nal_units(IDR_STREAM).find(|n| n.unit_type == 34).unwrap();
    let mut buf = [0u8; 128];
    let n = unescape(pps.raw, &mut buf).unwrap();
    let p = parse_pps(&buf[..n]).unwrap();
    assert_eq!((p.id, p.sps_id), (0, 0));
    assert!(!p.tiles_enabled);
    assert!(!p.scaling_list_data_present);
}

#[test]
fn the_idr_slice_header_points_at_its_data() {
    let mut sps_buf = [0u8; 128];
    let mut pps_buf = [0u8; 128];
    let mut slice_buf = [0u8; 4096];
    let mut sps = None;
    let mut pps = None;
    let mut slice = None;
    for nal in nal_units(IDR_STREAM) {
        match nal.unit_type {
            33 => {
                let n = unescape(nal.raw, &mut sps_buf).unwrap();
                sps = Some(parse_sps(&sps_buf[..n]).unwrap());
            }
            34 => {
                let n = unescape(nal.raw, &mut pps_buf).unwrap();
                pps = Some(parse_pps(&pps_buf[..n]).unwrap());
            }
            19 => {
                let n = unescape(nal.raw, &mut slice_buf).unwrap();
                slice = Some((nal, n));
            }
            _ => {}
        }
    }
    let (sps, pps) = (sps.unwrap(), pps.unwrap());
    let (nal, n) = slice.unwrap();
    let h = parse_slice_header(&slice_buf[..n], nal.unit_type, &sps, &pps).unwrap();
    assert!(h.first_slice_segment_in_pic);
    assert_eq!(h.pps_id, 0);
    assert_eq!(h.slice_type, SLICE_I);
    assert!(!h.dependent);
    assert_eq!(h.segment_address, 0);
    assert!(
        h.data_offset > 2 && h.data_offset < n,
        "data after the header, inside the unit"
    );
    // The data is the rest of the unit: its bytes are in the raw NAL unit.
    let raw_at = raw_index(nal.raw, h.data_offset).unwrap();
    assert!(raw_at > 0 && raw_at < nal.raw.len());
}

#[test]
fn a_slice_of_a_non_idr_picture_is_refused() {
    let sps = Sps {
        width: 64,
        height: 64,
        ..Sps::default()
    };
    let pps = Pps::default();
    let data = [0u8; 8];
    assert!(matches!(
        parse_slice_header(&data, 1, &sps, &pps),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn an_explicit_short_term_set_reads_its_pictures() {
    // num_negative_pics 2 (011), num_positive_pics 1 (010); negatives -1
    // (ue 0, used 1) and -3 (ue 1, used 0); positive +2 (ue 1, used 1).
    let data = pack("011 010 1 1 010 0 010 1");
    let mut r = BitReader::new(&data);
    let set = parse_st_rps(&mut r, 0, &StRps::default()).unwrap();
    assert_eq!((set.num_negative, set.num_positive), (2, 1));
    assert_eq!(&set.delta_poc_s0[..2], &[-1, -3]);
    assert_eq!(&set.used_s0[..2], &[true, false]);
    assert_eq!(set.delta_poc_s1[0], 2);
    assert!(set.used_s1[0]);
}

#[test]
fn a_predicted_short_term_set_derives_its_pictures() {
    // The previous set is {-1}. This set predicts with deltaRps = -1 and
    // keeps both entries (used flags 1 1): {-1, -2}, no positive pictures.
    let previous = StRps {
        num_negative: 1,
        delta_poc_s0: [-1; 16],
        used_s0: [true; 16],
        ..StRps::default()
    };
    let data = pack("1 1 1 1 1");
    let mut r = BitReader::new(&data);
    let set = parse_st_rps(&mut r, 1, &previous).unwrap();
    assert_eq!((set.num_negative, set.num_positive), (2, 0));
    assert_eq!(&set.delta_poc_s0[..2], &[-1, -2]);
}

/// Parses every unit of `stream` the way a decoder would, keeping the last SPS
/// and PPS; returns how many parses succeeded. Never panics, whatever the input.
fn parse_all(stream: &[u8]) -> usize {
    let mut buf = [0u8; 4096];
    let (mut sps, mut pps) = (None, None);
    let mut ok = 0;
    for nal in nal_units(stream) {
        let Ok(n) = unescape(nal.raw, &mut buf) else {
            continue;
        };
        let rbsp = &buf[..n];
        let parsed = match nal.unit_type {
            32 => parse_vps(rbsp).is_ok(),
            33 => {
                sps = parse_sps(rbsp).ok();
                sps.is_some()
            }
            34 => {
                pps = parse_pps(rbsp).ok();
                pps.is_some()
            }
            _ => match (&sps, &pps) {
                (Some(s), Some(p)) => {
                    let h = parse_slice_header(rbsp, nal.unit_type, s, p);
                    if let Ok(h) = &h {
                        assert!(h.data_offset <= rbsp.len());
                        assert!(raw_index(nal.raw, h.data_offset).is_some() || h.data_offset == n);
                    }
                    h.is_ok()
                }
                _ => false,
            },
        };
        ok += usize::from(parsed);
    }
    ok
}

/// The fixture's units as (start, end) byte ranges of the stream.
fn unit_ranges() -> Vec<(usize, usize)> {
    let base = IDR_STREAM.as_ptr() as usize;
    nal_units(IDR_STREAM)
        .map(|n| {
            let start = n.raw.as_ptr() as usize - base;
            (start, start + n.raw.len())
        })
        .collect()
}

#[test]
fn every_truncated_header_is_an_error_not_a_panic() {
    assert_eq!(parse_all(IDR_STREAM), 4, "VPS, SPS, PPS and the slice");
    let mut buf = [0u8; 4096];
    // Each parameter set and the slice header, cut at every byte.
    for nal in nal_units(IDR_STREAM) {
        let n = unescape(nal.raw, &mut buf).unwrap();
        let whole = &buf[..n];
        for cut in 0..n {
            let part = &whole[..cut];
            let result = match nal.unit_type {
                32 => parse_vps(part).map(|_| ()),
                33 => parse_sps(part).map(|_| ()),
                34 => parse_pps(part).map(|_| ()),
                _ => continue,
            };
            if cut < 4 {
                assert_eq!(
                    result,
                    Err(Error::Truncated),
                    "unit {} cut at {cut}",
                    nal.unit_type
                );
            }
        }
    }
    // The whole stream cut at every byte up to the slice data.
    let (slice_start, _) = *unit_ranges().last().unwrap();
    for cut in 0..slice_start + 64 {
        parse_all(&IDR_STREAM[..cut]);
    }
}

#[test]
fn every_corrupted_header_bit_is_an_error_or_a_value_not_a_panic() {
    // Flip each bit of the VPS, SPS and PPS, and of the slice header's first
    // bytes, one at a time, and parse the whole stream.
    let mut stream = IDR_STREAM.to_vec();
    let ranges = unit_ranges();
    let (slice_start, _) = *ranges.last().unwrap();
    for &(start, end) in &ranges {
        let end = if start == slice_start {
            start + 32
        } else {
            end
        };
        for byte in start..end {
            for bit in 0..8 {
                stream[byte] ^= 1 << bit;
                parse_all(&stream);
                stream[byte] ^= 1 << bit;
            }
        }
    }
    // Every value of each header byte.
    for &(start, end) in &ranges[..ranges.len() - 1] {
        for byte in start..end {
            let was = stream[byte];
            for value in 0..=255u8 {
                stream[byte] = value;
                parse_all(&stream);
            }
            stream[byte] = was;
        }
    }
}

#[test]
fn random_garbage_is_parsed_without_panicking() {
    // A fixed xorshift sequence: the same bytes on every run.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let sps_pps: Vec<u8> = {
        let ranges = unit_ranges();
        IDR_STREAM[..ranges[ranges.len() - 1].0].to_vec()
    };
    for round in 0..2000 {
        let len = (next() % 200) as usize;
        let mut garbage: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        // Half of the rounds put each unit type's header in front, so the
        // parsers past the NAL header see the garbage.
        if round % 2 == 0 && len >= 2 {
            garbage[0] = ((next() % 64) as u8) << 1;
            garbage[1] = 1;
        }
        let mut stream = sps_pps.clone();
        stream.extend_from_slice(&[0, 0, 1]);
        stream.extend_from_slice(&garbage);
        parse_all(&stream);
        let mut lone = std::vec![0, 0, 1];
        lone.extend_from_slice(&garbage);
        parse_all(&lone);
    }
}

#[test]
fn a_predicted_set_from_the_largest_set_is_read_without_overrunning() {
    // The previous set holds 16 negative and 16 positive pictures: the
    // prediction reads 33 flags pairs. All flags 1: every picture is kept,
    // which is more than 16 on one side, and is refused.
    let mut previous = StRps {
        num_negative: 16,
        num_positive: 16,
        ..StRps::default()
    };
    for i in 0..16 {
        previous.delta_poc_s0[i] = -(i as i32 + 1);
        previous.delta_poc_s1[i] = i as i32 + 1;
    }
    let data = [0xffu8; 16];
    let mut r = BitReader::new(&data);
    assert!(parse_st_rps(&mut r, 1, &previous).is_err());
    // Only the current picture kept (deltaRps -1): one negative picture.
    let mut bits = std::string::String::from("1 1 1");
    for _ in 0..32 {
        bits.push_str(" 0 0");
    }
    bits.push_str(" 1");
    let data = pack(&bits);
    let mut r = BitReader::new(&data);
    let set = parse_st_rps(&mut r, 1, &previous).unwrap();
    assert_eq!((set.num_negative, set.num_positive), (1, 0));
    assert_eq!(set.delta_poc_s0[0], -1);
}

#[test]
fn out_of_range_values_in_a_built_sps_are_refused_by_the_slice_parser() {
    let sps = Sps {
        width: u32::MAX,
        height: u32::MAX,
        log2_min_cb_minus3: 200,
        log2_diff_max_min_cb: 200,
        ..Sps::default()
    };
    let pps = Pps::default();
    // Not the first slice: the address needs the picture's size in CTBs.
    let data = pack("0 010011 000000 001  0 0 1 1111111111111111 11111111");
    assert!(parse_slice_header(&data, IDR_W_RADL, &sps, &pps).is_err());
}

/// Writes syntax elements MSB first, for hand-built units.
#[derive(Default)]
struct Writer {
    bits: Vec<bool>,
}

impl Writer {
    fn u(&mut self, n: u32, value: u64) -> &mut Self {
        for i in (0..n).rev() {
            self.bits.push(value >> i & 1 == 1);
        }
        self
    }

    fn ue(&mut self, value: u32) -> &mut Self {
        let v = u64::from(value) + 1;
        let len = 64 - v.leading_zeros();
        self.u(len - 1, 0).u(len, v)
    }

    fn se(&mut self, value: i32) -> &mut Self {
        let k = if value > 0 {
            2 * value as i64 - 1
        } else {
            -2 * value as i64
        };
        self.ue(k as u32)
    }

    fn bytes(&self) -> Vec<u8> {
        self.bits
            .chunks(8)
            .map(|c| {
                c.iter()
                    .enumerate()
                    .fold(0u8, |b, (i, &on)| b | u8::from(on) << (7 - i))
            })
            .collect()
    }
}

/// An SPS (type 33) up to its picture size, with one sub-layer.
fn sps_head(w: &mut Writer, width: u32, height: u32) {
    w.u(16, 0x4201)
        .u(4, 0)
        .u(3, 0)
        .u(1, 1)
        .u(32, 0)
        .u(32, 0)
        .u(32, 0);
    w.ue(0).ue(1).ue(width).ue(height);
}

#[test]
fn a_conformance_window_that_overflows_or_covers_the_picture_is_refused() {
    let mut w = Writer::default();
    sps_head(&mut w, 320, 240);
    w.u(1, 1).ue(0x8000_0000).ue(0).ue(0).ue(0).u(64, 0);
    assert_eq!(
        parse_sps(&w.bytes()),
        Err(Error::Invalid("conformance window"))
    );

    let mut w = Writer::default();
    sps_head(&mut w, 320, 240);
    w.u(1, 1).ue(80).ue(80).ue(0).ue(0).u(64, 0);
    assert_eq!(
        parse_sps(&w.bytes()),
        Err(Error::Invalid("conformance window"))
    );
}

#[test]
fn pps_values_outside_their_ranges_are_refused() {
    let pps = |init_qp: i32, cb: i32| {
        let mut w = Writer::default();
        w.u(16, 0x4401)
            .ue(0)
            .ue(0)
            .u(1, 0)
            .u(1, 0)
            .u(3, 0)
            .u(1, 0)
            .u(1, 0);
        w.ue(0)
            .ue(0)
            .se(init_qp)
            .u(1, 0)
            .u(1, 0)
            .u(1, 0)
            .se(cb)
            .se(0);
        // Ten flags off, log2_parallel_merge_level_minus2 0, no extension.
        w.u(10, 0).ue(0).u(1, 0);
        parse_pps(&w.bytes())
    };
    assert!(pps(0, 0).is_ok());
    assert_eq!(pps(1000, 0), Err(Error::Invalid("init_qp_minus26")));
    assert_eq!(pps(0, -13), Err(Error::Invalid("pps_cb_qp_offset")));
}
