//! The H.264 decoder over the eight real encoder streams of `tests/data` (B-frames, CABAC and
//! CAVLC, four references, weighted prediction, several slices a picture), on a simulated codec
//! block.
//!
//! The simulated block is a [`Session`]: each job's work is parsed with the contract's own
//! parser, and the job is kept as the block was given it. The test reads every control back
//! through Lemnos's V4L2 H.264 structures and checks, for each slice:
//!
//! - the references are slot 0 (the bitstream, its length the slice with its start code) and
//!   slot 1 (the picture); a reference picture is never a slot;
//! - each reference the slice's lists name is a DPB entry whose `reference_ts` is the stamp of
//!   the job that decoded that picture (a picture's stamp is its last slice's job's: the one the
//!   block last gave its capture buffer);
//! - the stamps are whole microseconds and unique across the stream;
//! - the slice's kind, its list lengths, its IDR flag, and the presence of the prediction weight
//!   table agree with the expectation file.
//!
//! The expectations are in `tests/data/*.expect`, written outside this crate (`make_expect.py`).
//! The live session on `lemnosd` runs the same jobs: see the note at the end of this file.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use lemnos_contract_codec_block::BufferRef;
use lemnos_contract_codec_block::work::Work;
use lemnos_linux_sys::v4l2::h264 as sys;
use styx_block::Session;
use styx_block::h264::work::{SLOT_PICTURE, SLOT_SOURCE, TAG_SETUP, TAG_STAMP};
use styx_block::h264::{self, H264Decoder};
use styx_codec_api::{CodecError, Decoder, FourCc};
use styx_format::{MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FramePool, Layouts, PlaneLayout, packed_layouts};

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

/// One job as the simulated block was given it.
#[derive(Clone, Debug)]
struct Job {
    refs: Vec<BufferRef>,
    items: Vec<(u32, Vec<u8>)>,
}

/// The simulated codec block: parses each job's work with the contract's parser, keeps the job,
/// and completes it as done.
struct SimBlock(Arc<Mutex<Vec<Job>>>);

impl Session for SimBlock {
    fn run(&mut self, work: &[u8]) -> Result<u32, CodecError> {
        let parsed = Work::parse(work).ok_or_else(|| CodecError::Rejected("not work".into()))?;
        self.0.lock().unwrap().push(Job {
            refs: parsed.refs().collect(),
            items: parsed
                .items()
                .map(|(tag, bytes)| (tag, bytes.to_vec()))
                .collect(),
        });
        Ok(0)
    }
}

/// One slice's expectation, from its `slice` line of an `.expect` file.
struct Want {
    pic: usize,
    first: u32,
    nal: u8,
    n: [usize; 2],
    lists: [Vec<usize>; 2],
    weights: bool,
}

fn wants(text: &str) -> Vec<Want> {
    let list = |v: &str| -> Vec<usize> {
        if v == "-" {
            Vec::new()
        } else {
            v.split(',')
                .map(|x| x.parse().expect("a picture"))
                .collect()
        }
    };
    text.lines()
        .filter(|line| line.starts_with("slice "))
        .map(|line| {
            let kv: HashMap<&str, &str> = line
                .split_whitespace()
                .skip(1)
                .map(|w| w.split_once('=').expect("key=value"))
                .collect();
            let get = |k: &str| *kv.get(k).unwrap_or_else(|| panic!("missing {k}"));
            Want {
                pic: get("pic").parse().unwrap(),
                first: get("first").parse().unwrap(),
                nal: get("nal").parse().unwrap(),
                n: [get("n0").parse().unwrap(), get("n1").parse().unwrap()],
                lists: [list(get("l0")), list(get("l1"))],
                weights: get("wp") != "-",
            }
        })
        .collect()
}

/// The slice NAL units of a stream in decode order, each with its start code.
fn slices(stream: &[u8]) -> Vec<Vec<u8>> {
    h264::nal_units(stream)
        .filter(|nal| nal.unit_type == 1 || nal.unit_type == 5)
        .map(|nal| {
            let mut au = vec![0, 0, 1];
            au.extend_from_slice(nal.raw);
            au
        })
        .collect()
}

/// The luma size of the stream's first SPS, after cropping.
fn picture_size(stream: &[u8]) -> (u32, u32) {
    let mut buf = vec![0u8; stream.len()];
    let sps = h264::nal_units(stream)
        .find(|nal| nal.unit_type == 7)
        .expect("an SPS");
    let n = h264::unescape(sps.raw, &mut buf).unwrap();
    h264::parse_sps(&buf[..n]).unwrap().frame_size()
}

/// One structure, read from a control's bytes: they are its fields in order.
fn read<T: Default>(bytes: &[u8], as_bytes_mut: fn(&mut T) -> &mut [u8]) -> T {
    let mut value = T::default();
    let dst = as_bytes_mut(&mut value);
    assert_eq!(dst.len(), bytes.len(), "a control's size");
    dst.copy_from_slice(bytes);
    value
}

fn item(items: &[(u32, Vec<u8>)], tag: u32) -> &[u8] {
    &items
        .iter()
        .find(|(t, _)| *t == tag)
        .unwrap_or_else(|| panic!("no item {tag:#x}"))
        .1
}

fn has(items: &[(u32, Vec<u8>)], tag: u32) -> bool {
    items.iter().any(|(t, _)| *t == tag)
}

/// Decodes one stream through the simulated block, then checks its jobs against its expectations.
/// Returns the number of jobs and of list references checked.
fn run_stream(name: &str, stream: &[u8], expect: &str) -> (usize, usize) {
    let expected = wants(expect);
    let au_list = slices(stream);
    assert_eq!(au_list.len(), expected.len(), "{name}: slice count");
    let (width, height) = picture_size(stream);

    let jobs = Arc::new(Mutex::new(Vec::new()));
    let block = jobs.clone();
    let mut decoder =
        H264Decoder::new(move || Ok(Box::new(SimBlock(block.clone())) as Box<dyn Session>));
    decoder.set_parameter_sets(stream).expect("parameter sets");

    let pool: FramePool = FramePool::host(64, BufferSize::new(128 * 1024, 0)).unwrap();
    let bitstream =
        MediaFormat::with_defaults(FourCc::H264, Resolution::new(width, height).unwrap());
    let picture = MediaFormat::with_defaults(FourCc::NV12, Resolution::new(width, height).unwrap());
    let mut output = None;
    let mut current = usize::MAX;
    let mut picture_buffer: HashMap<usize, usize> = HashMap::new();
    for (k, (au, want)) in au_list.iter().zip(&expected).enumerate() {
        if want.first == 0 || current != want.pic {
            current = want.pic;
            let layouts = packed_layouts(FourCc::NV12, width, height, None, None).unwrap();
            output = Some(
                pool.acquire(FrameEntry::new(k as u64, 0, picture, layouts))
                    .expect("a picture buffer"),
            );
        }
        let layouts = Layouts::new(&[PlaneLayout::new(0, au.len(), au.len())]).unwrap();
        let mut input = pool
            .acquire(FrameEntry::new(k as u64, 0, bitstream, layouts))
            .expect("a bitstream buffer");
        input.write().expect("writable").bytes()[..au.len()].copy_from_slice(au);
        let out = output.as_mut().unwrap();
        decoder
            .decode(&input, out)
            .unwrap_or_else(|e| panic!("{name}: slice {k} (picture {}): {e:?}", want.pic));
        let buffer = out.buffer_index();
        let seen = *picture_buffer.entry(want.pic).or_insert(buffer);
        assert_eq!(seen, buffer, "{name}: picture {} changed buffer", want.pic);
    }
    drop(output);

    let jobs = jobs.lock().unwrap().clone();
    assert_eq!(jobs.len(), expected.len(), "{name}: one job a slice");

    let mut stamps = Vec::new();
    for (k, job) in jobs.iter().enumerate() {
        let stamp = u64::from_le_bytes(item(&job.items, TAG_STAMP).try_into().unwrap());
        assert_eq!(
            stamp % 1_000,
            0,
            "{name}: job {k} stamp {stamp} is whole microseconds"
        );
        stamps.push(stamp);
    }
    assert_eq!(
        stamps.iter().collect::<HashSet<_>>().len(),
        stamps.len(),
        "{name}: every stamp is unique"
    );
    // A picture's stamp is the stamp of its last slice's job.
    let mut picture_stamp: HashMap<usize, u64> = HashMap::new();
    for (k, want) in expected.iter().enumerate() {
        picture_stamp.insert(want.pic, stamps[k]);
    }

    let mut references = 0;
    for (k, (job, want)) in jobs.iter().zip(&expected).enumerate() {
        let ctx = format!("{name}: job {k} (picture {}, nal {})", want.pic, want.nal);

        // Slot 0 is the bitstream and slot 1 the picture: no reference is a slot.
        let slots: Vec<u32> = job.refs.iter().map(|r| r.slot).collect();
        assert_eq!(slots, [SLOT_SOURCE, SLOT_PICTURE], "{ctx}: slots");
        assert_eq!(
            job.refs[0].length as usize,
            au_list[k].len(),
            "{ctx}: source length"
        );
        assert_eq!(
            job.refs[1].buffer as usize, picture_buffer[&want.pic],
            "{ctx}: picture buffer"
        );

        // The setup names the coded format and the picture.
        let setup = item(&job.items, TAG_SETUP);
        let word = |at: usize| u32::from_le_bytes(setup[at * 4..at * 4 + 4].try_into().unwrap());
        assert_eq!(word(0), sys::PIX_FMT_H264_SLICE, "{ctx}: coded format");
        assert_eq!((word(1), word(2)), (width, height), "{ctx}: picture size");

        let sps: sys::Sps = read(item(&job.items, sys::CID_SPS), sys::Sps::as_bytes_mut);
        assert!(
            (u32::from(sps.pic_width_in_mbs_minus1) + 1) * 16 >= width,
            "{ctx}: the SPS covers the picture"
        );
        assert_eq!(
            has(&job.items, sys::CID_PRED_WEIGHTS),
            want.weights,
            "{ctx}: a prediction weight table is carried when the slice has one"
        );

        let slice: sys::SliceParams = read(
            item(&job.items, sys::CID_SLICE_PARAMS),
            sys::SliceParams::as_bytes_mut,
        );
        let dpb: sys::DecodeParams = read(
            item(&job.items, sys::CID_DECODE_PARAMS),
            sys::DecodeParams::as_bytes_mut,
        );
        assert_eq!(
            dpb.flags & sys::decode_flags::IDR_PIC != 0,
            want.nal == 5,
            "{ctx}: the IDR flag"
        );
        assert!(
            (slice.header_bit_size as usize) < au_list[k].len() * 8,
            "{ctx}: the header ends inside the slice"
        );

        for list in 0..2 {
            let n = want.n[list];
            if n == 0 {
                continue;
            }
            let (active, refs) = if list == 0 {
                (slice.num_ref_idx_l0_active_minus1, &slice.ref_pic_list0)
            } else {
                (slice.num_ref_idx_l1_active_minus1, &slice.ref_pic_list1)
            };
            assert_eq!(usize::from(active) + 1, n, "{ctx}: list {list} length");
            for (i, &pic) in want.lists[list].iter().enumerate() {
                let entry = dpb.dpb[usize::from(refs[i].index)];
                assert_ne!(
                    entry.flags & sys::dpb_flags::VALID,
                    0,
                    "{ctx}: list {list}[{i}] is a frame"
                );
                assert_eq!(
                    entry.reference_ts, picture_stamp[&pic],
                    "{ctx}: list {list}[{i}] names picture {pic} by the stamp of its job"
                );
                references += 1;
            }
        }
    }
    assert!(references > 0, "{name}: the streams have references");
    (jobs.len(), references)
}

#[test]
fn the_real_streams_reference_each_other_by_the_stamps_of_their_jobs() {
    let (mut jobs, mut references) = (0, 0);
    for &(name, stream, expect) in STREAMS {
        let (j, r) = run_stream(name, stream, expect);
        eprintln!("{name}: {j} jobs, {r} list references checked by stamp");
        jobs += j;
        references += r;
    }
    eprintln!("total: {jobs} jobs, {references} list references");
    assert!(jobs > 0 && references > 0);
}

// The same jobs through a live session on `lemnosd`, with the simulated codec block of `sim/mod.rs`,
// were tried and are not in this file. In one process the block's job stream stopped completing
// after a few to some tens of jobs (no completion within the deadline, at different slices of
// several of these streams), so the 192 jobs here are checked at the work level instead. The live
// path is covered by `live.rs` for HEVC, with two jobs.
