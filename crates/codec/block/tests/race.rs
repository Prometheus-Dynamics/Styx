//! The live session's buffer ordering, on the simulated block.
//!
//! Lemnos's rule (`docs/streams.md`, "job-driven streams", at the pinned rev): a returned buffer is
//! reusable in a job from the moment the consumer releases it, and the block takes returned buffers
//! back before it judges a job that names one. So a job posted right after a release may name the
//! buffer just released and is served; a job that names a buffer the consumer still holds is
//! refused, and that refusal is reported at once and counted, never posted again.
//!
//! The simulated block takes returned buffers back before each job it reads, as the real server
//! does. Its [`sim::AWAIT_JOBS`] makes a serve wait for the job after its completion, so the next
//! job is always read after the return, in the same serve as the completion. Its
//! [`sim::STALE_REFRESH`] switch is the OLD behaviour (one look per serve): with it on, a job that
//! names a buffer returned after that look is refused, and the test shows that refusal reported at
//! once.
//!
//! The tests share the simulated block's switches and job log, so they run one at a time.

mod sim;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, atomic::Ordering};
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

/// The session, shared between the decoder (which owns a boxed [`Session`]) and the test, which
/// reads its counters once the decodes are done.
struct Shared(Arc<Mutex<LiveSession>>);

impl Session for Shared {
    fn run(&mut self, work: &[u8]) -> Result<u32, CodecError> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .run(work)
    }
}

/// What one run of decodes saw: each decode's result, and the session's counters at the end.
struct Run {
    results: Vec<Result<(), CodecError>>,
    refusals: u64,
    posts_refused: u64,
}

/// Decodes each bitstream on one session over the simulated block, in order.
fn decode_each(name: &str, bitstreams: &[Vec<u8>]) -> Run {
    let (service, socket) = sim::start(name);
    let client = Client::connect(&socket, "styx-block-test").expect("connect");
    let device = client
        .find(CODEC_TAG)
        .expect("list")
        .expect("a codec.block/1 device");
    let session = Arc::new(Mutex::new(
        LiveSession::open(&client, &device, Duration::from_secs(5)).expect("open"),
    ));
    let pool = session
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pool()
        .clone();
    let mut once = Some(Shared(Arc::clone(&session)));
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

    let (refusals, posts_refused) = {
        let session = session.lock().unwrap_or_else(PoisonError::into_inner);
        (session.refusals(), session.posts_refused())
    };
    drop(decoder);
    drop(session);
    drop(pool);
    drop(client);
    service.stop();
    Run {
        results,
        refusals,
        posts_refused,
    }
}

/// Two IDR access units: the fixture's, as the live test decodes them.
fn two_frames() -> Vec<Vec<u8>> {
    let au = sim::idr_access_unit();
    vec![au.clone(), au]
}

#[test]
fn a_job_naming_a_buffer_released_just_before_the_post_is_accepted_with_no_retry() {
    let _turn = turn();
    // Each completion is released, and only then does the next job get posted and read: the
    // second job may name the first job's result, which the block takes back before judging it.
    sim::AWAIT_JOBS.store(1, Ordering::SeqCst);
    let run = decode_each("race-served", &two_frames());
    assert!(
        run.results.iter().all(Result::is_ok),
        "decodes: {:?}",
        run.results
    );
    assert_eq!((run.refusals, run.posts_refused), (0, 0), "nothing refused");

    let jobs = sim::JOBS.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(jobs.len(), 2, "one job per frame, no retry");
    assert!(jobs.iter().all(|job| job.outcome == JobStatus::Done));
    // The second job names the buffer the first job's completion released.
    let released = jobs[0].result;
    assert!(
        jobs[1].refs.iter().any(|r| r.buffer == released),
        "job 2 names buffer {released}, released before its post: {:?}",
        jobs[1].refs
    );
}

#[test]
fn a_job_naming_a_buffer_the_block_has_not_taken_back_is_refused_at_once_and_counted() {
    let _turn = turn();
    // The old once-per-serve look: the block takes returned buffers back once per serve, so a
    // job that names a buffer released after that look is refused. The session must report that
    // refusal at once, count it, and not post the job again.
    sim::STALE_REFRESH.store(true, Ordering::SeqCst);
    sim::AWAIT_JOBS.store(1, Ordering::SeqCst);
    let run = decode_each("race-stale", &two_frames());
    assert!(
        matches!(run.results[1], Err(CodecError::Rejected(_))),
        "the refusal is reported to the caller at once: {:?}",
        run.results[1]
    );
    assert!(
        run.results[0].is_ok(),
        "the first job is served: {:?}",
        run.results[0]
    );
    assert_eq!(run.refusals, 1, "the refusal is counted");
    assert_eq!(
        run.posts_refused, 1,
        "and counted as a posted job the block refused"
    );

    let jobs = sim::JOBS.lock().unwrap_or_else(PoisonError::into_inner);
    let outcomes: Vec<JobStatus> = jobs.iter().map(|job| job.outcome).collect();
    assert_eq!(
        outcomes,
        vec![JobStatus::Done, JobStatus::Refused],
        "the refused job is posted once: no retry"
    );
}

#[test]
fn a_refusal_the_block_gives_is_reported_and_not_posted_again() {
    let _turn = turn();
    // The block refuses the first job whatever it names: the refusal is the block's answer and
    // is reported at once, counted, and the job is not posted again.
    sim::REFUSE_JOBS.store(1, Ordering::SeqCst);
    let run = decode_each("race-genuine", &two_frames()[..1]);
    assert!(
        matches!(run.results[0], Err(CodecError::Rejected(_))),
        "a refusal is reported: {:?}",
        run.results[0]
    );
    assert_eq!((run.refusals, run.posts_refused), (1, 1), "counted once");

    let jobs = sim::JOBS.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(jobs.len(), 1, "the job is not posted again");
    assert_eq!(jobs[0].outcome, JobStatus::Refused);
}
