//! The H.264 parser and reference tracker over real encoder output. Each stream's expectations
//! are in `tests/data/*.expect`, written by `tests/data/make_expect.py` from FFmpeg's CBS trace
//! (the syntax), a model of 8.2.4 and 8.2.5 (the reference lists and marking), and the POC order
//! (the display order). `tests/data/README.md` gives the encoders, the commands and the versions.
//! None of the expectations is read from this crate.

use styx_block::h264::poc::Pic;
use styx_block::h264::{
    Dpb, Mmco, NONE, PocState, Pps, RplMod, SliceHeader, SliceKind, Sps, nal_units, parse_pps,
    parse_slice_header, parse_sps, peek_pps_id, unescape,
};

const STREAMS: &[(&str, &[u8], &str)] = &[
    (
        "vaapi_high_bf2_cabac",
        include_bytes!("data/vaapi_high_bf2_cabac.h264"),
        include_str!("data/vaapi_high_bf2_cabac.expect"),
    ),
    (
        "vaapi_main_p_cavlc",
        include_bytes!("data/vaapi_main_p_cavlc.h264"),
        include_str!("data/vaapi_main_p_cavlc.expect"),
    ),
    (
        "nvenc_high_bf3_each_cabac",
        include_bytes!("data/nvenc_high_bf3_each_cabac.h264"),
        include_str!("data/nvenc_high_bf3_each_cabac.expect"),
    ),
    (
        "nvenc_main_refs4_cavlc",
        include_bytes!("data/nvenc_main_refs4_cavlc.h264"),
        include_str!("data/nvenc_main_refs4_cavlc.expect"),
    ),
    (
        "nvenc_high_p_wp_cabac",
        include_bytes!("data/nvenc_high_p_wp_cabac.h264"),
        include_str!("data/nvenc_high_p_wp_cabac.expect"),
    ),
    (
        "nvenc_main_p_wp_cavlc",
        include_bytes!("data/nvenc_main_p_wp_cavlc.h264"),
        include_str!("data/nvenc_main_p_wp_cavlc.expect"),
    ),
    (
        "vaapi_gst_b2_refs4_slices3_cabac",
        include_bytes!("data/vaapi_gst_b2_refs4_slices3_cabac.h264"),
        include_str!("data/vaapi_gst_b2_refs4_slices3_cabac.expect"),
    ),
    (
        "vaapi_gst_b2_refs4_slices3_cavlc",
        include_bytes!("data/vaapi_gst_b2_refs4_slices3_cavlc.h264"),
        include_str!("data/vaapi_gst_b2_refs4_slices3_cavlc.expect"),
    ),
];

/// One slice's expectation, from a `slice` line of an `.expect` file.
#[derive(Debug)]
struct Slice {
    pic: usize,
    first: u32,
    nal: u8,
    reference: u8,
    kind: String,
    frame_num: u32,
    poc: i32,
    n: [u8; 2],
    lists: [Vec<i64>; 2],
    mmco: String,
    mods: [String; 2],
    wp: String,
    cabac: bool,
    data: Option<usize>,
}

fn list_of(text: &str) -> Vec<i64> {
    if text == "-" {
        return Vec::new();
    }
    text.split(',')
        .map(|v| v.parse().expect("list entry"))
        .collect()
}

fn text_of(text: &str) -> String {
    if text == "-" {
        String::new()
    } else {
        text.to_string()
    }
}

/// Reads an `.expect` file: the slices in decode order, then the display order of the pictures.
fn expectations(text: &str) -> (Vec<Slice>, Vec<usize>) {
    let mut slices = Vec::new();
    let mut display = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("slice") => {
                let kv: std::collections::HashMap<&str, &str> = words
                    .map(|w| w.split_once('=').expect("key=value"))
                    .collect();
                let get = |k: &str| *kv.get(k).unwrap_or_else(|| panic!("missing {k}"));
                slices.push(Slice {
                    pic: get("pic").parse().unwrap(),
                    first: get("first").parse().unwrap(),
                    nal: get("nal").parse().unwrap(),
                    reference: get("ref").parse().unwrap(),
                    kind: get("kind").to_string(),
                    frame_num: get("frame_num").parse().unwrap(),
                    poc: get("poc").parse().unwrap(),
                    n: [get("n0").parse().unwrap(), get("n1").parse().unwrap()],
                    lists: [list_of(get("l0")), list_of(get("l1"))],
                    mmco: text_of(get("mmco")),
                    mods: [text_of(get("mod0")), text_of(get("mod1"))],
                    wp: text_of(get("wp")),
                    cabac: get("cabac") == "1",
                    data: match get("data") {
                        "-" => None,
                        v => Some(v.parse().unwrap()),
                    },
                });
            }
            Some("display") => {
                display = words
                    .map(|w| w.strip_prefix("pic=").unwrap().parse().unwrap())
                    .collect();
            }
            _ => {}
        }
    }
    (slices, display)
}

/// The slice kind as the expectation writes it.
fn kind_name(kind: SliceKind) -> &'static str {
    match kind {
        SliceKind::P => "P",
        SliceKind::B => "B",
        SliceKind::I => "I",
        SliceKind::SP => "SP",
        SliceKind::SI => "SI",
    }
}

/// `op:arg` per memory management operation, as the expectation writes them (MMCO 3 as `d/idx`).
fn mmco_text(ops: &[Mmco]) -> String {
    ops.iter()
        .map(|m| match m.op {
            3 => format!("3:{}/{}", m.a, m.b),
            4 | 6 => format!("{}:{}", m.op, m.b),
            _ => format!("{}:{}", m.op, m.a),
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// The prediction weight table as the expectation writes it: `d{luma}:{chroma}`, then per list
/// (one for P, two for B) its entries, `L{weight}:{offset}` or `L-`, then `C{cb_w}:{cb_o}:{cr_w}:
/// {cr_o}` or `C-`. Empty when the slice has no table.
fn weights_text(h: &SliceHeader) -> String {
    let Some(w) = h.weights.as_deref() else {
        return String::new();
    };
    let mut out = format!("d{}:{}", w.luma_log2_denom, w.chroma_log2_denom);
    let lists = if h.kind == SliceKind::B { 2 } else { 1 };
    for list in 0..lists {
        out.push(';');
        let entries: Vec<String> = (0..usize::from(h.num_ref_idx_active[list]))
            .map(|i| {
                let e = w.entries[list][i];
                let luma = match e.luma {
                    Some((weight, offset)) => format!("L{weight}:{offset}"),
                    None => "L-".to_string(),
                };
                let chroma = match e.chroma {
                    Some([(cbw, cbo), (crw, cro)]) => format!("C{cbw}:{cbo}:{crw}:{cro}"),
                    None => "C-".to_string(),
                };
                format!("{luma}{chroma}")
            })
            .collect();
        out.push_str(&entries.join(","));
    }
    out
}

/// `idc:value` per list modification.
fn mods_text(mods: &[RplMod]) -> String {
    mods.iter()
        .map(|m| format!("{}:{}", m.idc, m.value))
        .collect::<Vec<_>>()
        .join(";")
}

/// A picture whose slices are being decoded: its first slice's header, which carries the
/// marking, and its counts. It is marked when the next picture starts.
struct Pending {
    header: SliceHeader,
    sps: Sps,
    pic: Pic,
    picture: usize,
}

/// Decodes a stream's syntax, reference lists and display order with this crate's parser, and
/// checks each against the expectation. A picture's buffer is its number in decode order, so a
/// list entry (a DPB slot) is compared as the number of the picture it holds.
fn check_stream(name: &str, stream: &[u8], expect: &str) {
    let (expected, display) = expectations(expect);
    let mut spss: Vec<Sps> = Vec::new();
    let mut ppss: Vec<Pps> = Vec::new();
    let mut buf = vec![0u8; stream.len()];
    let mut dpb = Dpb::new();
    let mut pocs = PocState::default();
    let mut pending: Option<Pending> = None;
    let mut pictures: Vec<(i32, usize)> = Vec::new();
    let mut expected_slices = expected.iter();
    let mut slice_count = 0usize;

    fn finish(dpb: &mut Dpb, pocs: &mut PocState, pending: &mut Option<Pending>) {
        if let Some(p) = pending.take() {
            dpb.mark(&p.header, &p.sps, p.picture as u32, p.pic.stored());
            pocs.commit(&p.header, &p.pic);
        }
    }

    for nal in nal_units(stream) {
        let n = unescape(nal.raw, &mut buf).expect("unescape");
        let rbsp = &buf[..n];
        match nal.unit_type {
            7 => spss.push(parse_sps(rbsp).unwrap_or_else(|e| panic!("{name}: SPS: {e}"))),
            8 => {
                let pps = parse_pps(rbsp, |id| {
                    spss.iter()
                        .find(|s| s.id == id)
                        .map(|s| s.chroma_format_idc)
                })
                .unwrap_or_else(|e| panic!("{name}: PPS: {e}"));
                ppss.push(pps);
            }
            1 | 5 => {
                let e = expected_slices
                    .next()
                    .unwrap_or_else(|| panic!("{name}: more slices than expected"));
                let ctx = format!("{name}, slice {slice_count} (picture {})", e.pic);
                slice_count += 1;
                assert_eq!(nal.unit_type, e.nal, "{ctx}: nal_unit_type");
                let pps_id = peek_pps_id(rbsp).unwrap();
                let pps = ppss.iter().find(|p| p.id == pps_id).expect("PPS");
                let sps = spss.iter().find(|s| s.id == pps.sps_id).expect("SPS");
                let h = parse_slice_header(rbsp, nal.unit_type, nal.nal_ref_idc, sps, pps)
                    .unwrap_or_else(|e| panic!("{ctx}: header: {e}"));
                assert_eq!(h.first_mb_in_slice, e.first, "{ctx}: first_mb_in_slice");
                assert_eq!(h.nal_ref_idc, e.reference, "{ctx}: nal_ref_idc");
                assert_eq!(kind_name(h.kind), e.kind, "{ctx}: slice kind");
                assert_eq!(h.frame_num, e.frame_num, "{ctx}: frame_num");
                assert_eq!(pps.cabac, e.cabac, "{ctx}: entropy_coding_mode_flag");
                assert_eq!(h.num_ref_idx_active, e.n, "{ctx}: num_ref_idx_active");
                assert_eq!(
                    mods_text(h.modifications(0)),
                    e.mods[0],
                    "{ctx}: list 0 modifications"
                );
                assert_eq!(
                    mods_text(h.modifications(1)),
                    e.mods[1],
                    "{ctx}: list 1 modifications"
                );
                assert_eq!(
                    mmco_text(h.mmcos()),
                    e.mmco,
                    "{ctx}: memory management operations"
                );
                assert_eq!(weights_text(&h), e.wp, "{ctx}: prediction weight table");
                if let Some(bit) = e.data {
                    assert_eq!(h.data_bit_offset, bit, "{ctx}: slice data bit offset");
                }

                // A slice with first_mb_in_slice 0 starts a picture: the previous one is marked.
                if h.first_mb_in_slice == 0 {
                    finish(&mut dpb, &mut pocs, &mut pending);
                    dpb.check_frame_num(&h, sps)
                        .unwrap_or_else(|e| panic!("{ctx}: {e}"));
                    let pic = pocs
                        .compute(&h, sps)
                        .unwrap_or_else(|e| panic!("{ctx}: {e}"));
                    pictures.push((pic.poc.frame(), e.pic));
                    pending = Some(Pending {
                        header: h.clone(),
                        sps: sps.clone(),
                        pic,
                        picture: e.pic,
                    });
                }
                let current = pending.as_ref().expect("a picture for this slice");
                assert_eq!(current.picture, e.pic, "{ctx}: picture number");
                assert_eq!(current.pic.poc.frame(), e.poc, "{ctx}: picture order count");
                let lists = dpb
                    .lists(&h, sps, current.pic.poc)
                    .unwrap_or_else(|e| panic!("{ctx}: {e}"));
                for list in 0..2 {
                    let got: Vec<i64> = lists
                        .list(list)
                        .iter()
                        .map(|&slot| {
                            assert_ne!(slot, NONE, "{ctx}: list {list} has an empty entry");
                            dpb.get(usize::from(slot))
                                .expect("slot holds a frame")
                                .buffer as i64
                        })
                        .collect();
                    assert_eq!(got, e.lists[list], "{ctx}: reference picture list {list}");
                }
            }
            _ => {}
        }
    }
    finish(&mut dpb, &mut pocs, &mut pending);
    assert!(
        expected_slices.next().is_none(),
        "{name}: fewer slices than expected"
    );
    pictures.sort_by_key(|&(poc, _)| poc);
    let order: Vec<usize> = pictures.iter().map(|&(_, picture)| picture).collect();
    assert_eq!(order, display, "{name}: output (display) order");
}

#[test]
fn real_streams_match_their_expectations() {
    for &(name, stream, expect) in STREAMS {
        check_stream(name, stream, expect);
    }
}
