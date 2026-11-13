//! The H.264 decoder over a simulated block session: the job's work, the controls it carries
//! (read back with Lemnos's own V4L2 H.264 structures), the references a P frame names by the
//! stamp of the job that decoded them, and the recovery after a failed or refused job.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use lemnos_contract_codec_block::BufferRef;
use lemnos_contract_codec_block::work::Work;
use lemnos_linux_sys::v4l2::h264 as sys;
use styx_block::h264::work::{SLOT_PICTURE, SLOT_SOURCE, TAG_SETUP, TAG_STAMP};
use styx_block::h264::{self, H264Decoder};
use styx_block::{Session, backoff_ns};
use styx_codec_api::{
    Backend, CodecError, CodecKind, Decoder, FourCc, FrameLease, Implementation, Registry, Request,
};
use styx_format::{MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FramePool, Layouts, PlaneLayout, packed_layouts};
use styx_h264::{StyxH264, StyxH264Input};

const STREAM: &[u8] = include_bytes!("../src/h264/fixtures/openh264_cbp.h264");

#[derive(Clone, Copy)]
enum Step {
    Done,
    Refuse,
    Fail,
}

struct Seen {
    refs: Vec<BufferRef>,
    items: Vec<(u32, Vec<u8>)>,
}

#[derive(Default)]
struct State {
    opens: u32,
    script: VecDeque<Step>,
    jobs: Vec<Seen>,
}

struct Sim(Arc<Mutex<State>>);

impl Session for Sim {
    fn run(&mut self, work: &[u8]) -> Result<u32, CodecError> {
        let parsed = Work::parse(work).ok_or_else(|| CodecError::Rejected("not work".into()))?;
        let mut state = self.0.lock().unwrap();
        state.jobs.push(Seen {
            refs: parsed.refs().collect(),
            items: parsed
                .items()
                .map(|(tag, bytes)| (tag, bytes.to_vec()))
                .collect(),
        });
        match state.script.pop_front().unwrap_or(Step::Done) {
            Step::Done => Ok(0),
            Step::Refuse => Err(CodecError::Rejected(
                "refused by the simulated block".into(),
            )),
            Step::Fail => Err(CodecError::Failed("the simulated block failed".into())),
        }
    }
}

fn decoder(state: &Arc<Mutex<State>>) -> H264Decoder {
    let state = state.clone();
    H264Decoder::new(move || {
        state.lock().unwrap().opens += 1;
        Ok(Box::new(Sim(state.clone())) as Box<dyn Session>)
    })
}

/// The access units of the fixture's slices: each one's start code and slice NAL unit.
fn fixture_slices() -> Vec<Vec<u8>> {
    h264::nal_units(STREAM)
        .filter(|nal| nal.unit_type == 1 || nal.unit_type == 5)
        .map(|nal| {
            let mut au = vec![0, 0, 1];
            au.extend_from_slice(nal.raw);
            au
        })
        .collect()
}

fn pool() -> FramePool {
    FramePool::host(32, BufferSize::new(64 * 1024, 0)).unwrap()
}

fn bitstream(pool: &FramePool, au: &[u8], stamp: u64) -> FrameLease {
    let layouts = Layouts::new(&[PlaneLayout::new(0, au.len(), au.len())]).unwrap();
    let format = MediaFormat::with_defaults(FourCc::H264, Resolution::new(96, 64).unwrap());
    let mut lease = pool
        .acquire(FrameEntry::new(stamp, stamp, format, layouts))
        .unwrap();
    lease.write().unwrap().bytes()[..au.len()].copy_from_slice(au);
    lease
}

fn picture(pool: &FramePool, stamp: u64, width: u32, height: u32) -> FrameLease {
    let layouts = packed_layouts(FourCc::NV12, width, height, None, None).unwrap();
    let format = MediaFormat::with_defaults(FourCc::NV12, Resolution::new(width, height).unwrap());
    pool.acquire(FrameEntry::new(stamp, stamp, format, layouts))
        .unwrap()
}

fn item(seen: &Seen, tag: u32) -> &[u8] {
    seen.items
        .iter()
        .find(|(t, _)| *t == tag)
        .map(|(_, bytes)| bytes.as_slice())
        .unwrap_or_else(|| panic!("no item {tag:#x}"))
}

fn tags(seen: &Seen) -> Vec<u32> {
    seen.items.iter().map(|(tag, _)| *tag).collect()
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at * 4..at * 4 + 4].try_into().unwrap())
}

/// One of the structures, read from a control's bytes: they are its fields in order.
fn read<T: Default>(bytes: &[u8], as_bytes_mut: fn(&mut T) -> &mut [u8]) -> T {
    let mut value = T::default();
    let dst = as_bytes_mut(&mut value);
    assert_eq!(dst.len(), bytes.len(), "a control's size");
    dst.copy_from_slice(bytes);
    value
}

/// Decodes one slice, keeping its picture held as a reference would be. Returns the picture's
/// buffer number.
fn decode(
    decoder: &mut H264Decoder,
    pool: &FramePool,
    held: &mut Vec<FrameLease>,
    au: &[u8],
    stamp: u64,
) -> Result<usize, CodecError> {
    decode_sized(decoder, pool, held, au, stamp, (96, 64))
}

fn decode_sized(
    decoder: &mut H264Decoder,
    pool: &FramePool,
    held: &mut Vec<FrameLease>,
    au: &[u8],
    stamp: u64,
    (width, height): (u32, u32),
) -> Result<usize, CodecError> {
    let input = bitstream(pool, au, stamp);
    let mut output = picture(pool, stamp, width, height);
    let index = output.buffer_index();
    decoder.decode(&input, &mut output)?;
    held.push(output);
    Ok(index)
}

fn assert_stamp_ok(stamp: u64) {
    assert_eq!(
        stamp % 1_000,
        0,
        "a stamp is a whole number of microseconds"
    );
    assert_ne!(stamp, 0);
}

#[test]
fn an_idr_slice_runs_one_job_with_its_controls_and_no_references() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state);
    decoder.set_parameter_sets(STREAM).unwrap();
    let pool = pool();
    let mut held = Vec::new();
    let slices = fixture_slices();
    let index = decode(&mut decoder, &pool, &mut held, &slices[0], 7).unwrap();

    let state = state.lock().unwrap();
    assert_eq!((state.opens, state.jobs.len()), (1, 1));
    let seen = &state.jobs[0];
    let slots: Vec<u32> = seen.refs.iter().map(|r| r.slot).collect();
    assert_eq!(slots, [SLOT_SOURCE, SLOT_PICTURE]);
    assert_eq!(seen.refs[0].length as usize, slices[0].len());
    assert_eq!(seen.refs[1].buffer as usize, index);

    // Setup: the H.264 slice format in, 96x64 NV12 out, one plane.
    let setup = item(seen, TAG_SETUP);
    assert_eq!(word(setup, 0), sys::PIX_FMT_H264_SLICE);
    assert_eq!((word(setup, 1), word(setup, 2)), (96, 64));
    assert_eq!(word(setup, 3), FourCc::NV12.to_u32());
    assert_eq!(word(setup, 4), 1);
    // The stamp is the decoder's, a whole number of microseconds: the first job's.
    let stamp = u64::from_le_bytes(item(seen, TAG_STAMP).try_into().unwrap());
    assert_eq!(stamp, 1_000);

    // The controls: slice based, Annex B, the parameter sets, then the slice and its DPB.
    let tags = tags(seen);
    assert_eq!(tags[..2], [TAG_SETUP, TAG_STAMP]);
    assert_eq!(
        tags[2..6],
        [
            sys::CID_DECODE_MODE,
            sys::CID_START_CODE,
            sys::CID_SPS,
            sys::CID_PPS
        ]
    );
    assert_eq!(
        tags[tags.len() - 2..],
        [sys::CID_SLICE_PARAMS, sys::CID_DECODE_PARAMS]
    );
    assert_eq!(
        word(item(seen, sys::CID_DECODE_MODE), 0) as i32,
        sys::DECODE_MODE_SLICE_BASED
    );
    assert_eq!(
        word(item(seen, sys::CID_START_CODE), 0) as i32,
        sys::START_CODE_ANNEX_B
    );

    let sps: sys::Sps = read(item(seen, sys::CID_SPS), sys::Sps::as_bytes_mut);
    assert_eq!(
        sps.pic_width_in_mbs_minus1, 5,
        "96 luma samples is 6 macroblocks"
    );
    assert_eq!(sps.pic_height_in_map_units_minus1, 3, "64 is 4 macroblocks");
    assert_eq!(sps.profile_idc, 66, "the fixture is a baseline stream");

    let slice: sys::SliceParams = read(
        item(seen, sys::CID_SLICE_PARAMS),
        sys::SliceParams::as_bytes_mut,
    );
    assert_eq!(slice.slice_type, sys::SLICE_TYPE_I);
    assert_ne!(slice.header_bit_size, 0);
    assert_eq!(slice.num_ref_idx_l0_active_minus1, 0);

    let decode: sys::DecodeParams = read(
        item(seen, sys::CID_DECODE_PARAMS),
        sys::DecodeParams::as_bytes_mut,
    );
    assert_ne!(decode.flags & sys::decode_flags::IDR_PIC, 0);
    assert!(
        decode
            .dpb
            .iter()
            .all(|e| e.flags & sys::dpb_flags::VALID == 0),
        "an IDR names no reference"
    );
    drop(state);

    // The picture is in the tracker once the next picture starts, or on flush.
    assert_eq!(decoder.dpb().frames().count(), 0);
    decoder.flush();
    let dpb = decoder.dpb();
    assert_eq!(dpb.frames().count(), 1);
    assert_eq!(dpb.get(0).unwrap().stamp_ns, 1_000);
}

#[test]
fn a_p_slice_names_its_reference_by_the_stamp_of_the_job_that_decoded_it() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state);
    decoder.set_parameter_sets(STREAM).unwrap();
    let pool = pool();
    let mut held = Vec::new();
    let slices = fixture_slices();
    decode(&mut decoder, &pool, &mut held, &slices[0], 1).unwrap();
    decode(&mut decoder, &pool, &mut held, &slices[1], 2).unwrap();

    let state = state.lock().unwrap();
    assert_eq!(state.jobs.len(), 2);
    let idr_stamp = u64::from_le_bytes(item(&state.jobs[0], TAG_STAMP).try_into().unwrap());
    let p = &state.jobs[1];
    let slice: sys::SliceParams = read(
        item(p, sys::CID_SLICE_PARAMS),
        sys::SliceParams::as_bytes_mut,
    );
    let decode: sys::DecodeParams = read(
        item(p, sys::CID_DECODE_PARAMS),
        sys::DecodeParams::as_bytes_mut,
    );
    assert_eq!(slice.slice_type, sys::SLICE_TYPE_P);
    assert_eq!(slice.num_ref_idx_l0_active_minus1, 0);

    // List 0's one entry is a DPB index, and that entry is the IDR, named by its stamp.
    let entry = slice.ref_pic_list0[0];
    assert_eq!(entry.fields, sys::FRAME_REF);
    let dpb_entry = decode.dpb[usize::from(entry.index)];
    assert_ne!(dpb_entry.flags & sys::dpb_flags::VALID, 0);
    assert_ne!(dpb_entry.flags & sys::dpb_flags::ACTIVE, 0);
    assert_eq!(dpb_entry.reference_ts, idr_stamp);
    assert_eq!(dpb_entry.frame_num, 0);
    // The job's own stamp is not the reference: it is the next one.
    let own = u64::from_le_bytes(item(p, TAG_STAMP).try_into().unwrap());
    assert_eq!(own, idr_stamp + 1_000);
    assert_stamp_ok(own);
}

#[test]
fn every_frame_is_a_job_with_a_unique_stamp_and_each_p_frame_names_the_previous_one() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state);
    decoder.set_parameter_sets(STREAM).unwrap();
    let pool = pool();
    let mut held = Vec::new();
    for au in fixture_slices().iter() {
        decode(&mut decoder, &pool, &mut held, au, 0).unwrap();
    }
    let state = state.lock().unwrap();
    assert_eq!(state.jobs.len(), 9);

    let stamps: Vec<u64> = state
        .jobs
        .iter()
        .map(|job| u64::from_le_bytes(item(job, TAG_STAMP).try_into().unwrap()))
        .collect();
    assert!(stamps.iter().all(|&s| s % 1_000 == 0 && s != 0));
    assert_eq!(stamps.iter().collect::<HashSet<_>>().len(), stamps.len());

    for i in 1..9 {
        let slice: sys::SliceParams = read(
            item(&state.jobs[i], sys::CID_SLICE_PARAMS),
            sys::SliceParams::as_bytes_mut,
        );
        let decode: sys::DecodeParams = read(
            item(&state.jobs[i], sys::CID_DECODE_PARAMS),
            sys::DecodeParams::as_bytes_mut,
        );
        if i % 4 == 0 {
            assert_eq!(slice.slice_type, sys::SLICE_TYPE_I, "frame {i}");
        } else {
            let entry = slice.ref_pic_list0[0];
            assert_eq!(
                decode.dpb[usize::from(entry.index)].reference_ts,
                stamps[i - 1],
                "frame {i} names frame {}",
                i - 1
            );
        }
    }
    drop(state);
    decoder.flush();
    assert_eq!(decoder.dpb().frames().count(), 1);
}

#[test]
fn a_failed_job_leaves_the_tracker_as_it_was_and_backs_off() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state);
    decoder.set_parameter_sets(STREAM).unwrap();
    let pool = pool();
    let mut held = Vec::new();
    let slices = fixture_slices();
    decode(&mut decoder, &pool, &mut held, &slices[0], 1).unwrap();
    state.lock().unwrap().script = VecDeque::from([Step::Fail]);
    let err = decode(&mut decoder, &pool, &mut held, &slices[1], 2).unwrap_err();
    assert!(matches!(err, CodecError::Failed(_)));
    assert_eq!(decoder.failures(), 1);
    assert!(!decoder.is_open());
    // The IDR completed, so it is in the tracker; the P frame did not complete, so it is not.
    let dpb = decoder.dpb();
    assert_eq!(dpb.frames().count(), 1);
    assert_eq!(dpb.get(0).unwrap().stamp_ns, 1_000);
    // Inside the backoff nothing reaches the block.
    let again = decode(&mut decoder, &pool, &mut held, &slices[1], 3).unwrap_err();
    assert!(matches!(again, CodecError::Unavailable(_)));
    assert_eq!(state.lock().unwrap().jobs.len(), 2);
    assert!(backoff_ns(0) > 0);
}

#[test]
fn a_refused_job_changes_nothing_and_keeps_the_session() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state);
    decoder.set_parameter_sets(STREAM).unwrap();
    let pool = pool();
    let mut held = Vec::new();
    state.lock().unwrap().script = VecDeque::from([Step::Refuse]);
    let err = decode(&mut decoder, &pool, &mut held, &fixture_slices()[0], 1).unwrap_err();
    assert!(matches!(err, CodecError::Rejected(_)));
    assert!(decoder.is_open());
    assert_eq!(decoder.failures(), 0);
    decoder.flush();
    assert_eq!(decoder.dpb().frames().count(), 0);
}

#[test]
fn a_slice_the_parser_refuses_never_reaches_the_block() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state);
    decoder.set_parameter_sets(STREAM).unwrap();
    let pool = pool();
    let mut held = Vec::new();
    // An SPS as the input: not a slice.
    let sps = h264::nal_units(STREAM).find(|n| n.unit_type == 7).unwrap();
    let mut au = vec![0, 0, 1];
    au.extend_from_slice(sps.raw);
    assert!(matches!(
        decode(&mut decoder, &pool, &mut held, &au, 1),
        Err(CodecError::Rejected(_))
    ));
    assert_eq!(state.lock().unwrap().jobs.len(), 0);
}

#[test]
fn a_p_slice_with_no_reference_is_refused() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state);
    decoder.set_parameter_sets(STREAM).unwrap();
    let pool = pool();
    let mut held = Vec::new();
    // The stream's second slice, with no IDR before it.
    let err = decode(&mut decoder, &pool, &mut held, &fixture_slices()[1], 1).unwrap_err();
    assert!(matches!(err, CodecError::Rejected(_)), "{err:?}");
    assert_eq!(state.lock().unwrap().jobs.len(), 0);
}

#[test]
fn the_candidate_is_chosen_as_a_native_h264_decoder() {
    let state = Arc::new(Mutex::new(State::default()));
    let open = {
        let state = state.clone();
        move || Ok(Box::new(Sim(state.clone())) as Box<dyn Session>)
    };
    let mut registry = Registry::<()>::new();
    registry.register(h264::candidate(open));
    let selection = registry
        .select(Request::new(CodecKind::Decoder, FourCc::H264, FourCc::NV12))
        .unwrap();
    let Implementation::Decoder(codec) = selection.into_implementation() else {
        panic!("expected a decoder");
    };
    let descriptor = codec.descriptor();
    assert_eq!(descriptor.name, "h264-v4l2-stateless");
    assert_eq!(descriptor.backend, Backend::Native);
}

#[test]
fn an_encoded_stream_from_styx_h264_decodes_frame_by_frame() {
    // Styx's own encoder: five gray frames, an IDR every three. Each frame's slice is decoded
    // with the parameter sets of the first frame.
    let (width, height) = (48usize, 32usize);
    let mut encoder = StyxH264::new(28, 3);
    let y = vec![96u8; width * height];
    let mut parameter_sets = None;
    let mut slices = Vec::new();
    let mut kinds = Vec::new();
    for _ in 0..5 {
        let mut out = Vec::new();
        let input = StyxH264Input::Gray {
            y: &y,
            width,
            height,
            stride: width,
        };
        let idr = encoder.encode(&input, &mut out).unwrap();
        if parameter_sets.is_none() {
            parameter_sets = Some(out.clone());
        }
        kinds.push(idr);
        for nal in h264::nal_units(&out).filter(|n| n.unit_type == 1 || n.unit_type == 5) {
            let mut au = vec![0, 0, 1];
            au.extend_from_slice(nal.raw);
            slices.push(au);
        }
    }
    assert_eq!(kinds, [true, false, false, true, false]);

    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state);
    decoder
        .set_parameter_sets(&parameter_sets.unwrap())
        .unwrap();
    let pool = pool();
    let mut held = Vec::new();
    for (i, au) in slices.iter().enumerate() {
        decode_sized(&mut decoder, &pool, &mut held, au, i as u64, (48, 32)).unwrap();
    }
    assert_eq!(state.lock().unwrap().jobs.len(), 5);
    // The IDR of frame 3 is in the tracker; frame 4 is the picture still open.
    assert_eq!(decoder.dpb().frames().count(), 1);
}
