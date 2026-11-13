//! The HEVC decoder over a simulated block session: the job's work, what the block is given,
//! and the recovery after a failed job. The simulated session reads the work back with the
//! contract's own parser, so what it checks is what the block would be given.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lemnos_contract_codec_block::BufferRef;
use lemnos_contract_codec_block::work::Work;
use lemnos_linux_sys::v4l2::hevc as sys;
use styx_block::hevc::{self, controls};
use styx_block::work::{SLOT_PICTURE, SLOT_SOURCE, TAG_SETUP, TAG_STAMP};
use styx_block::{HevcDecoder, Session, backoff_ns, candidate};
use styx_codec_api::{
    CodecError, CodecKind, Decoder, FourCc, FrameLease, Implementation, Registry, Request,
};
use styx_format::{MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FramePool, Layouts, PlaneLayout, packed_layouts};

const STREAM: &[u8] = include_bytes!("../src/hevc/fixtures/idr320.h265");

#[derive(Clone, Copy)]
enum Step {
    Done,
    Refuse,
    Fail,
}

/// One job as the simulated block saw it.
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

fn decoder(state: &Arc<Mutex<State>>) -> HevcDecoder {
    let state = state.clone();
    HevcDecoder::new(move || {
        state.lock().unwrap().opens += 1;
        Ok(Box::new(Sim(state.clone())) as Box<dyn Session>)
    })
}

fn stream_nals(stream: &[u8]) -> Vec<(u8, Vec<u8>)> {
    hevc::nal_units(stream)
        .map(|nal| (nal.unit_type, nal.raw.to_vec()))
        .collect()
}

/// The access unit of one IDR frame: the stream's IDR slice, with its start code, first.
fn idr_access_unit() -> Vec<u8> {
    let (_, raw) = stream_nals(STREAM)
        .into_iter()
        .find(|(unit_type, _)| *unit_type == 19)
        .expect("the fixture has an IDR slice");
    let mut au = vec![0, 0, 1];
    au.extend_from_slice(&raw);
    au
}

fn pool(payload: usize) -> FramePool {
    FramePool::host(4, BufferSize::new(payload, 0)).unwrap()
}

fn bitstream(pool: &FramePool, au: &[u8], stamp: u64) -> FrameLease {
    let layouts = Layouts::new(&[PlaneLayout::new(0, au.len(), au.len())]).unwrap();
    let format = MediaFormat::with_defaults(FourCc::HEVC, Resolution::new(320, 240).unwrap());
    let mut lease = pool
        .acquire(FrameEntry::new(stamp, stamp, format, layouts))
        .unwrap();
    lease.write().unwrap().bytes()[..au.len()].copy_from_slice(au);
    lease
}

fn picture(pool: &FramePool, stamp: u64) -> FrameLease {
    let layouts = packed_layouts(FourCc::NV12, 320, 240, None, None).unwrap();
    let format = MediaFormat::with_defaults(FourCc::NV12, Resolution::new(320, 240).unwrap());
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

#[test]
fn an_idr_frame_runs_one_job_with_its_setup_and_controls() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state);
    decoder.set_parameter_sets(STREAM).unwrap();

    let au = idr_access_unit();
    let pool = pool(128 * 1024);
    let input = bitstream(&pool, &au, 5_000);
    let mut output = picture(&pool, 5_000);
    decoder.decode(&input, &mut output).unwrap();

    let state = state.lock().unwrap();
    assert_eq!(state.opens, 1);
    assert_eq!(state.jobs.len(), 1);
    let seen = &state.jobs[0];

    // The slice is the source, from the start of the input buffer; the picture is the output.
    let source = seen.refs.iter().find(|r| r.slot == SLOT_SOURCE).unwrap();
    assert_eq!(source.buffer as usize, input.buffer_index());
    assert_eq!((source.offset, source.length as usize), (0, au.len()));
    let picture = seen.refs.iter().find(|r| r.slot == SLOT_PICTURE).unwrap();
    assert_eq!(picture.buffer as usize, output.buffer_index());
    assert_eq!(seen.refs.len(), 2);

    // Setup: S265 in, 320x240 NV12 out, one plane.
    let setup = item(seen, TAG_SETUP);
    assert_eq!(setup.len(), 20);
    assert_eq!(word(setup, 0), sys::PIX_FMT_HEVC_SLICE);
    assert_eq!((word(setup, 1), word(setup, 2)), (320, 240));
    assert_eq!(word(setup, 3), FourCc::NV12.to_u32());
    assert_eq!(word(setup, 4), 1);

    // The stamp is the decoder's: the first job's, a whole number of microseconds.
    assert_eq!(item(seen, TAG_STAMP), 1_000u64.to_le_bytes());

    // The SPS and PPS controls are the parsed headers, in the kernel's layout.
    let nals = stream_nals(STREAM);
    let mut buf = vec![0u8; 4096];
    let sps_raw = &nals.iter().find(|(t, _)| *t == 33).unwrap().1;
    let n = hevc::unescape(sps_raw, &mut buf).unwrap();
    let sps = hevc::parse_sps(&buf[..n]).unwrap();
    assert_eq!(
        item(seen, sys::CID_SPS),
        controls::sps_control(&sps).unwrap().as_bytes()
    );
    let pps_raw = &nals.iter().find(|(t, _)| *t == 34).unwrap().1;
    let n = hevc::unescape(pps_raw, &mut buf).unwrap();
    let pps = hevc::parse_pps(&buf[..n]).unwrap();
    assert_eq!(
        item(seen, sys::CID_PPS),
        controls::pps_control(&pps).unwrap().as_bytes()
    );
    assert_eq!(
        item(seen, sys::CID_DECODE_MODE),
        sys::DECODE_MODE_FRAME_BASED.to_le_bytes()
    );
    assert_eq!(
        item(seen, sys::CID_START_CODE),
        sys::START_CODE_ANNEX_B.to_le_bytes()
    );
    let slice = item(seen, sys::CID_SLICE_PARAMS);
    assert_eq!(slice.len(), std::mem::size_of::<sys::SliceParams>());
    assert_eq!(
        slice[std::mem::offset_of!(sys::SliceParams, nal_unit_type)],
        19
    );
    let decode = item(seen, sys::CID_DECODE_PARAMS);
    assert_eq!(decode.len(), std::mem::size_of::<sys::DecodeParams>());
}

#[test]
fn a_second_frame_reuses_the_open_session() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state);
    decoder.set_parameter_sets(STREAM).unwrap();
    let au = idr_access_unit();
    let pool = pool(128 * 1024);
    for stamp in [10, 20] {
        let input = bitstream(&pool, &au, stamp);
        let mut output = picture(&pool, stamp);
        decoder.decode(&input, &mut output).unwrap();
    }
    let state = state.lock().unwrap();
    assert_eq!(state.opens, 1);
    assert_eq!(state.jobs.len(), 2);
    assert_eq!(item(&state.jobs[0], TAG_STAMP), 1_000u64.to_le_bytes());
    assert_eq!(item(&state.jobs[1], TAG_STAMP), 2_000u64.to_le_bytes());
}

#[test]
fn a_failed_job_backs_off_then_reopens_the_session() {
    let state = Arc::new(Mutex::new(State::default()));
    state.lock().unwrap().script.push_back(Step::Fail);
    let mut decoder = decoder(&state);
    decoder.set_parameter_sets(STREAM).unwrap();
    let au = idr_access_unit();
    let pool = pool(128 * 1024);

    let input = bitstream(&pool, &au, 1);
    let mut output = picture(&pool, 1);
    assert!(matches!(
        decoder.decode(&input, &mut output),
        Err(CodecError::Failed(_))
    ));
    assert!(!decoder.is_open());
    assert_eq!(decoder.failures(), 1);

    // Within the backoff: refused without a job, and the block is not asked again.
    assert!(matches!(
        decoder.decode(&input, &mut output),
        Err(CodecError::Unavailable(_))
    ));
    assert_eq!(state.lock().unwrap().jobs.len(), 1);

    std::thread::sleep(Duration::from_millis(backoff_ns(0) / 1_000_000 + 30));
    decoder.decode(&input, &mut output).unwrap();
    let state = state.lock().unwrap();
    assert_eq!(state.opens, 2);
    assert_eq!(state.jobs.len(), 2);
    drop(state);
    assert_eq!(decoder.failures(), 0);
}

#[test]
fn a_refused_job_keeps_the_session_and_sets_no_backoff() {
    let state = Arc::new(Mutex::new(State::default()));
    state.lock().unwrap().script.push_back(Step::Refuse);
    let mut decoder = decoder(&state);
    decoder.set_parameter_sets(STREAM).unwrap();
    let au = idr_access_unit();
    let pool = pool(128 * 1024);
    let input = bitstream(&pool, &au, 1);
    let mut output = picture(&pool, 1);
    assert!(matches!(
        decoder.decode(&input, &mut output),
        Err(CodecError::Rejected(_))
    ));
    assert!(decoder.is_open());
    assert_eq!(decoder.failures(), 0);
    decoder.decode(&input, &mut output).unwrap();
    assert_eq!(state.lock().unwrap().opens, 1);
}

#[test]
fn an_inter_slice_is_refused_before_the_block_sees_it() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state);
    decoder.set_parameter_sets(STREAM).unwrap();
    // A TRAIL_R slice (unit type 1): its reference picture sets are not parsed.
    let au = [0, 0, 1, 0x02, 0x01, 0xaf, 0x40, 0x12, 0x34];
    let pool = pool(128 * 1024);
    let input = bitstream(&pool, &au, 1);
    let mut output = picture(&pool, 1);
    assert!(matches!(
        decoder.decode(&input, &mut output),
        Err(CodecError::Rejected(_))
    ));
    let state = state.lock().unwrap();
    assert_eq!(state.jobs.len(), 0);
    assert_eq!(state.opens, 0);
}

#[test]
fn a_frame_without_parameter_sets_is_refused() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut decoder = decoder(&state);
    let au = idr_access_unit();
    let pool = pool(128 * 1024);
    let input = bitstream(&pool, &au, 1);
    let mut output = picture(&pool, 1);
    assert!(matches!(
        decoder.decode(&input, &mut output),
        Err(CodecError::Rejected(_))
    ));
    assert_eq!(state.lock().unwrap().opens, 0);
}

#[test]
fn the_backoff_starts_at_250_ms_doubles_and_stops_at_8_s() {
    let ms = |attempt| backoff_ns(attempt) / 1_000_000;
    assert_eq!(
        (0..8).map(ms).collect::<Vec<_>>(),
        vec![250, 500, 1000, 2000, 4000, 8000, 8000, 8000]
    );
}

#[test]
fn the_candidate_is_chosen_as_a_native_hevc_decoder() {
    let state = Arc::new(Mutex::new(State::default()));
    let open = {
        let state = state.clone();
        move || Ok(Box::new(Sim(state.clone())) as Box<dyn Session>)
    };
    let mut registry = Registry::<()>::new();
    registry.register(candidate(open));
    let selection = registry
        .select(Request::new(CodecKind::Decoder, FourCc::HEVC, FourCc::NV12))
        .unwrap();
    let Implementation::Decoder(codec) = selection.into_implementation() else {
        panic!("expected a decoder");
    };
    let descriptor = codec.descriptor();
    assert_eq!(descriptor.name, "hevc-v4l2-stateless");
    assert_eq!(descriptor.backend, styx_codec_api::Backend::Native);
}
