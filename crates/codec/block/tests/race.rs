//! The live session's job race, forced into its bad ordering on the simulated block.
//!
//! A job's result buffer is returned to the block when its completion is released, and the
//! session's next job can name that buffer (the frame pool hands out the buffer that was just
//! returned). The block takes returned buffers back when it looks at the stream. The simulated
//! block's [`sim::AWAIT_JOBS`] makes a serve wait for the job after its completion, so the next job
//! is always read after the return, in the same serve as the completion: the ordering the
//! session's test (`live.rs`) met as a refusal once in a gate run.
//!
//! The tests share the simulated block's switches and job log, so they run one at a time.

mod sim;

use std::sync::{Mutex, MutexGuard, PoisonError, atomic::Ordering};
use std::time::Duration;

use lemnos_contract_codec_block::JobStatus;
use styx_block::live::CODEC_TAG;
use styx_block::{HevcDecoder, LiveSession, Session};
use styx_codec_api::{CodecError, Decoder, FourCc};
use styx_format::{MediaFormat, Resolution};
use styx_frame::{FrameEntry, Layouts, PlaneLayout, packed_layouts};
use styx_lemnos::Client;

static SERIAL: Mutex<()> = Mutex::new(());

/// Holds the tests' turn and resets the simulated block's switches and job log.
fn turn() -> MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    sim::STALE_REFRESH.store(false, Ordering::SeqCst);
    sim::AWAIT_JOBS.store(0, Ordering::SeqCst);
    sim::REFUSE_JOBS.store(0, Ordering::SeqCst);
    sim::JOBS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    guard
}

/// Decodes each bitstream on a fresh session over the simulated block, in order, and returns each
/// decode's result.
fn decode_each(name: &str, bitstreams: &[Vec<u8>]) -> Vec<Result<(), CodecError>> {
    let (service, socket) = sim::start(name);
    let client = Client::connect(&socket, "styx-block-test").expect("connect");
    let device = client
        .find(CODEC_TAG)
        .expect("list")
        .expect("a codec.block/1 device");
    let session = LiveSession::open(&client, &device, Duration::from_secs(5)).expect("open");
    let pool = session.pool().clone();
    let mut once = Some(session);
    let mut decoder = HevcDecoder::new(move || {
        once.take()
            .map(|s| Box::new(s) as Box<dyn Session>)
            .ok_or_else(|| CodecError::Unavailable("the test's one session is in use".into()))
    });
    decoder
        .set_parameter_sets(sim::STREAM)
        .expect("parameter sets");

    let bitstream = MediaFormat::with_defaults(FourCc::HEVC, Resolution::new(320, 240).unwrap());
    let picture = MediaFormat::with_defaults(FourCc::NV12, Resolution::new(320, 240).unwrap());
    let mut results = Vec::new();
    for (i, au) in bitstreams.iter().enumerate() {
        let frame = i as u64 + 1;
        let stamp = 5_000 + 10 * i as u64;
        let layouts = Layouts::new(&[PlaneLayout::new(0, au.len(), au.len())]).unwrap();
        let mut input = pool
            .acquire(FrameEntry::new(frame, stamp, bitstream, layouts))
            .expect("bitstream buffer");
        input.write().expect("writable").bytes()[..au.len()].copy_from_slice(au);
        let layouts = packed_layouts(FourCc::NV12, 320, 240, None, None).unwrap();
        let mut output = pool
            .acquire(FrameEntry::new(frame, stamp, picture, layouts))
            .expect("picture buffer");
        results.push(decoder.decode(&input, &mut output).map(|_| ()));
    }

    drop(decoder);
    drop(pool);
    drop(client);
    service.stop();
    results
}

/// Two IDR access units: the fixture's, as the live test decodes them.
fn two_frames() -> Vec<Vec<u8>> {
    let au = sim::idr_access_unit();
    vec![au.clone(), au]
}

#[test]
fn a_job_naming_a_buffer_returned_just_before_is_served_when_the_block_takes_returns_per_job() {
    let _turn = turn();
    sim::AWAIT_JOBS.store(1, Ordering::SeqCst);
    let results = decode_each("race-served", &two_frames());
    assert!(results.iter().all(Result::is_ok), "decodes: {results:?}");

    let jobs = sim::JOBS.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(jobs.len(), 2, "one job per frame, no retry");
    assert!(jobs.iter().all(|job| job.outcome == JobStatus::Done));
    // The second job names the first job's result: the buffer returned in between.
    let first_result = jobs[0].refs.len();
    assert_eq!(first_result, 2);
}

#[test]
fn a_refusal_caused_by_a_return_is_posted_again_and_then_served() {
    let _turn = turn();
    sim::STALE_REFRESH.store(true, Ordering::SeqCst);
    sim::AWAIT_JOBS.store(1, Ordering::SeqCst);
    let results = decode_each("race-retry", &two_frames());
    assert!(results.iter().all(Result::is_ok), "decodes: {results:?}");

    let jobs = sim::JOBS.lock().unwrap_or_else(PoisonError::into_inner);
    let outcomes: Vec<JobStatus> = jobs.iter().map(|job| job.outcome).collect();
    assert_eq!(
        outcomes,
        vec![JobStatus::Done, JobStatus::Refused, JobStatus::Done],
        "the refused job is posted again, and the posting is served"
    );
    assert_eq!(
        jobs[1].refs, jobs[2].refs,
        "the retry names what the refused job named"
    );
}

#[test]
fn a_refusal_the_session_did_not_cause_is_reported_and_not_posted_again() {
    let _turn = turn();
    // The block refuses the first job whatever it names. Nothing the session returned is named
    // (it has returned nothing yet), so the refusal is the block's answer, not the session's
    // ordering: it is reported at once.
    sim::REFUSE_JOBS.store(1, Ordering::SeqCst);
    let results = decode_each("race-genuine", &two_frames()[..1]);
    assert!(
        matches!(results[0], Err(CodecError::Rejected(_))),
        "a refusal is reported: {:?}",
        results[0]
    );

    let jobs = sim::JOBS.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(jobs.len(), 1, "the job is not posted again");
    assert_eq!(jobs[0].outcome, JobStatus::Refused);
}
