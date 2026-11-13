//! The H.264 decoder over a simulated block session: the job's work, what the block is given,
//! the reference buffers a P frame names, the tracker after each job, and the recovery after a
//! failed or refused job. The simulated session reads the work back with the contract's own
//! parser, as the HEVC test does.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use lemnos_contract_codec_block::BufferRef;
use lemnos_contract_codec_block::work::Work;
use styx_block::h264::controls::{Controls, Layout, SliceView};
use styx_block::h264::work::{REFERENCE_SLOT, SLOT_PICTURE, SLOT_SOURCE, TAG_SETUP, TAG_STAMP};
use styx_block::h264::{self, H264Decoder};
use styx_block::{Session, backoff_ns};
use styx_codec_api::{
    Backend, CodecError, CodecKind, Decoder, FourCc, FrameLease, Implementation, Registry, Request,
};
use styx_format::{MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FramePool, Layouts, PlaneLayout, packed_layouts};
use styx_h264::{StyxH264, StyxH264Input};

const STREAM: &[u8] = include_bytes!("../src/h264/fixtures/openh264_cbp.h264");
/// The control the test layout writes: the slice's type, frame number and list 0's DPB slots.
const LAYOUT_TAG: u32 = 0x0098_0100;

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

/// The test layout: writes the slice's type, frame number and list 0 (DPB slots) as one control.
struct Lists;

impl Layout for Lists {
    fn write(&mut self, slice: &SliceView<'_>, out: &mut Controls) -> Result<(), CodecError> {
        let mut bytes = vec![slice.header.nal_unit_type];
        bytes.extend_from_slice(&slice.header.frame_num.to_le_bytes());
        bytes.push(slice.lists.len[0]);
        bytes.extend_from_slice(slice.lists.list(0));
        out.push(LAYOUT_TAG, &bytes);
        Ok(())
    }
}

/// A layout that refuses every slice, as a layout that cannot express the controls does.
struct Refuse;

impl Layout for Refuse {
    fn write(&mut self, _: &SliceView<'_>, _: &mut Controls) -> Result<(), CodecError> {
        Err(CodecError::Rejected(
            "this layout writes no controls".into(),
        ))
    }
}

fn decoder(state: &Arc<Mutex<State>>, layout: impl Layout + 'static) -> H264Decoder {
    let state = state.clone();
    H264Decoder::new(
        move || {
            state.lock().unwrap().opens += 1;
            Ok(Box::new(Sim(state.clone())) as Box<dyn Session>)
        },
        layout,
    )
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
    FramePool::host(16, BufferSize::new(64 * 1024, 0)).unwrap()
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

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at * 4..at * 4 + 4].try_into().unwrap())
}

/// Decodes one access unit, keeping its picture held as a reference would be. Returns the
/// picture's buffer number.
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

#[test]
fn an_idr_frame_runs_one_job_with_no_reference_pictures() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state, Lists);
    decoder.set_parameter_sets(STREAM).unwrap();
    let pool = pool();
    let mut held = Vec::new();
    let slices = fixture_slices();
    let index = decode(&mut decoder, &pool, &mut held, &slices[0], 1_000).unwrap();

    let state = state.lock().unwrap();
    assert_eq!((state.opens, state.jobs.len()), (1, 1));
    let seen = &state.jobs[0];
    let slots: Vec<u32> = seen.refs.iter().map(|r| r.slot).collect();
    assert_eq!(slots, [SLOT_SOURCE, SLOT_PICTURE]);
    assert_eq!(seen.refs[1].buffer as usize, index);
    // Setup: the H.264 slice format in, 96x64 NV12 out, one plane.
    let setup = item(seen, TAG_SETUP);
    assert_eq!(word(setup, 0), u32::from_le_bytes(*b"S264"));
    assert_eq!((word(setup, 1), word(setup, 2)), (96, 64));
    assert_eq!(word(setup, 3), FourCc::NV12.to_u32());
    assert_eq!(word(setup, 4), 1);
    assert_eq!(item(seen, TAG_STAMP), 1_000u64.to_le_bytes());
    // The layout's control: an IDR (nal_unit_type 5), frame 0, no list entries.
    assert_eq!(item(seen, LAYOUT_TAG), [5, 0, 0, 0, 0, 0]);
    drop(state);
    let dpb = decoder.dpb();
    assert_eq!(dpb.frames().count(), 1);
    assert_eq!(dpb.get(0).unwrap().buffer as usize, index);
}

#[test]
fn a_p_frame_names_its_reference_by_slot_and_buffer() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state, Lists);
    decoder.set_parameter_sets(STREAM).unwrap();
    let pool = pool();
    let mut held = Vec::new();
    let slices = fixture_slices();
    let idr = decode(&mut decoder, &pool, &mut held, &slices[0], 1).unwrap();
    decode(&mut decoder, &pool, &mut held, &slices[1], 2).unwrap();

    let state = state.lock().unwrap();
    assert_eq!(state.jobs.len(), 2);
    let p = &state.jobs[1];
    // The P frame's list 0 is the one DPB slot, and that slot's reference is the IDR's buffer.
    assert_eq!(item(p, LAYOUT_TAG), [1, 1, 0, 0, 0, 1, 0]);
    let reference = p
        .refs
        .iter()
        .find(|r| r.slot == REFERENCE_SLOT)
        .expect("the reference slot of DPB slot 0");
    assert_eq!(reference.buffer as usize, idr);
}

#[test]
fn the_gop_decodes_every_frame_and_each_picture_is_the_next_frames_reference() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state, Lists);
    decoder.set_parameter_sets(STREAM).unwrap();
    let pool = pool();
    let mut held = Vec::new();
    let mut pictures = Vec::new();
    for (i, au) in fixture_slices().iter().enumerate() {
        pictures.push(decode(&mut decoder, &pool, &mut held, au, i as u64).unwrap());
    }
    let state = state.lock().unwrap();
    assert_eq!(state.jobs.len(), 9);
    for i in 1..9 {
        let reference = state.jobs[i].refs.iter().find(|r| r.slot == REFERENCE_SLOT);
        if i % 4 == 0 {
            // An IDR takes no reference.
            assert!(reference.is_none(), "frame {i}");
        } else {
            assert_eq!(
                reference.unwrap().buffer as usize,
                pictures[i - 1],
                "frame {i}"
            );
        }
    }
    drop(state);
    assert_eq!(decoder.dpb().frames().count(), 1);
}

#[test]
fn a_failed_job_leaves_the_tracker_as_it_was_and_backs_off() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state, Lists);
    decoder.set_parameter_sets(STREAM).unwrap();
    let pool = pool();
    let mut held = Vec::new();
    let slices = fixture_slices();
    let idr = decode(&mut decoder, &pool, &mut held, &slices[0], 1).unwrap();
    state.lock().unwrap().script = VecDeque::from([Step::Fail]);
    let err = decode(&mut decoder, &pool, &mut held, &slices[1], 2).unwrap_err();
    assert!(matches!(err, CodecError::Failed(_)));
    assert_eq!(decoder.failures(), 1);
    assert!(!decoder.is_open());
    // The P frame did not enter the tracker: the IDR is still the one reference.
    let dpb = decoder.dpb();
    assert_eq!(dpb.frames().count(), 1);
    assert_eq!(dpb.get(0).unwrap().buffer as usize, idr);
    // Inside the backoff nothing reaches the block.
    let again = decode(&mut decoder, &pool, &mut held, &slices[1], 3).unwrap_err();
    assert!(matches!(again, CodecError::Unavailable(_)));
    assert_eq!(state.lock().unwrap().jobs.len(), 2);
    assert!(backoff_ns(0) > 0);
}

#[test]
fn a_refused_job_changes_nothing_and_keeps_the_session() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state, Lists);
    decoder.set_parameter_sets(STREAM).unwrap();
    let pool = pool();
    let mut held = Vec::new();
    state.lock().unwrap().script = VecDeque::from([Step::Refuse]);
    let err = decode(&mut decoder, &pool, &mut held, &fixture_slices()[0], 1).unwrap_err();
    assert!(matches!(err, CodecError::Rejected(_)));
    assert!(decoder.is_open());
    assert_eq!(decoder.failures(), 0);
    assert_eq!(decoder.dpb().frames().count(), 0);
}

#[test]
fn a_slice_the_parser_refuses_never_reaches_the_block() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state, Lists);
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
    let mut decoder = decoder(&state, Lists);
    decoder.set_parameter_sets(STREAM).unwrap();
    let pool = pool();
    let mut held = Vec::new();
    // The stream's second slice, with no IDR before it.
    let err = decode(&mut decoder, &pool, &mut held, &fixture_slices()[1], 1).unwrap_err();
    assert!(matches!(err, CodecError::Rejected(_)), "{err:?}");
    assert_eq!(state.lock().unwrap().jobs.len(), 0);
}

#[test]
fn a_layout_that_refuses_runs_no_job() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state, Refuse);
    decoder.set_parameter_sets(STREAM).unwrap();
    let pool = pool();
    let mut held = Vec::new();
    assert!(matches!(
        decode(&mut decoder, &pool, &mut held, &fixture_slices()[0], 1),
        Err(CodecError::Rejected(_))
    ));
    assert_eq!(state.lock().unwrap().jobs.len(), 0);
    assert_eq!(decoder.dpb().frames().count(), 0);
}

#[test]
fn the_candidate_is_chosen_as_a_native_h264_decoder() {
    let state = Arc::new(Mutex::new(State::default()));
    let open = {
        let state = state.clone();
        move || Ok(Box::new(Sim(state.clone())) as Box<dyn Session>)
    };
    let mut registry = Registry::<()>::new();
    registry.register(h264::candidate(open, || Box::new(Lists) as Box<dyn Layout>));
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
    let mut decoder = decoder(&state, Lists);
    decoder
        .set_parameter_sets(&parameter_sets.unwrap())
        .unwrap();
    let pool = pool();
    let mut held = Vec::new();
    for (i, au) in slices.iter().enumerate() {
        decode_sized(&mut decoder, &pool, &mut held, au, i as u64, (48, 32)).unwrap();
    }
    assert_eq!(state.lock().unwrap().jobs.len(), 5);
    assert_eq!(decoder.dpb().frames().count(), 1);
}
