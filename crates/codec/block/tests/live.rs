//! The live session on Lemnos's simulated backend, through the real socket.
//!
//! `lemnosd` runs in this process with a simulated `codec.block/1` block (see `sim/mod.rs`) on
//! `lemnos-sim`. The session finds the device by tag, opens its job stream writable, and the
//! HEVC decoder runs two IDR frames through it. Each picture comes back as a `FrameLease` over
//! the job stream's memory, with no copy.

mod sim;

use std::path::PathBuf;
use std::time::Duration;

use lemnos_contract_codec_block::{CodecBlock, JobStatus};
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_block::hevc;
use styx_block::live::CODEC_TAG;
use styx_block::{HevcDecoder, LiveSession, Session};
use styx_codec_api::{CodecError, Decoder, FourCc};
use styx_format::{MediaFormat, Resolution};
use styx_frame::{FrameEntry, Layouts, PlaneLayout, copy_counters, packed_layouts};
use styx_lemnos::Client;

const STREAM: &[u8] = include_bytes!("../src/hevc/fixtures/idr320.h265");

const BOARD: &str = "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-codec\"\n\
    [[devices]]\nid = \"codec\"\ndriver = \"sim-codec\"\n\
    [devices.streams.jobs]\nbuffers = 8\nbuffer_bytes = 131072\njob_slots = 8\njob_bytes = 20\n";

fn socket_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-block-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start(name: &str) -> (Service, PathBuf) {
    let dir = socket_dir().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("sock");
    let config = Config {
        socket: socket.clone(),
        name: "styx-block-test".to_string(),
        board: parse(BOARD).expect("board"),
        drivers: vec![entry::<sim::SimCodec>()],
        backend: Box::new(SimBoard::new()),
        options: Options {
            call_deadline: Duration::from_millis(500),
            ..Options::default()
        },
    };
    (Service::start(config).expect("start lemnosd"), socket)
}

/// The access unit of one IDR frame: the stream's IDR slice, with its start code, first.
fn idr_access_unit() -> Vec<u8> {
    let (_, raw) = hevc::nal_units(STREAM)
        .map(|nal| (nal.unit_type, nal.raw.to_vec()))
        .find(|(unit_type, _)| *unit_type == 19)
        .expect("the fixture has an IDR slice");
    let mut au = vec![0, 0, 1];
    au.extend_from_slice(&raw);
    au
}

#[test]
fn decode_jobs_run_on_the_simulated_block_and_the_pictures_come_back_with_no_copies() {
    let (service, socket) = start("decode");
    let client = Client::connect(&socket, "styx-block-test").expect("connect");
    let device = client
        .find(CODEC_TAG)
        .expect("list")
        .expect("a codec.block/1 device");
    let session = LiveSession::open(&client, &device, Duration::from_secs(5)).expect("open");
    assert_eq!(session.stream_name(), "jobs");

    // The pool is the session's; the decoder runs on the session, and the leases come from the
    // same pool. A clone of the pool is a handle to it.
    let pool = session.pool().clone();
    let mut once = Some(session);
    let mut decoder = HevcDecoder::new(move || {
        once.take()
            .map(|s| Box::new(s) as Box<dyn Session>)
            .ok_or_else(|| CodecError::Unavailable("the test's one session is in use".into()))
    });
    decoder.set_parameter_sets(STREAM).expect("parameter sets");

    let au = idr_access_unit();
    let bitstream = MediaFormat::with_defaults(FourCc::HEVC, Resolution::new(320, 240).unwrap());
    let picture = MediaFormat::with_defaults(FourCc::NV12, Resolution::new(320, 240).unwrap());
    let (width, height) = (320usize, 240usize);
    let total = width * height * 3 / 2;

    for (frame, stamp) in [(1u64, 5_000u64), (2, 5_010)] {
        let layouts = Layouts::new(&[PlaneLayout::new(0, au.len(), au.len())]).unwrap();
        let mut input = pool
            .acquire(FrameEntry::new(frame, stamp, bitstream, layouts))
            .expect("bitstream buffer");
        input.write().expect("writable").bytes()[..au.len()].copy_from_slice(&au);

        let layouts = packed_layouts(FourCc::NV12, 320, 240, None, None).unwrap();
        let mut output = pool
            .acquire(FrameEntry::new(frame, stamp, picture, layouts))
            .expect("picture buffer");
        decoder
            .decode(&input, &mut output)
            .expect("decode through the live session");

        // The job's stamp is the decoder's, read back from what the block was given.
        let job_stamp = {
            let jobs = sim::JOBS.lock().unwrap();
            let bytes = &jobs.last().expect("the job").items[1].1;
            u64::from_le_bytes(bytes[..8].try_into().expect("a stamp"))
        };
        assert_eq!(job_stamp % 1_000, 0);

        // The picture is the block's: the pattern of this job's stamp, over the whole frame.
        let read = output.read().expect("mapped for the CPU");
        let y = read.plane(0).expect("luma").data;
        let uv = read.plane(1).expect("chroma").data;
        let expected: Vec<u8> = (0..total).map(|i| sim::pattern(i, job_stamp)).collect();
        assert_eq!(&y[..width * height], &expected[..width * height]);
        assert_eq!(&uv[..width * height / 2], &expected[width * height..]);
        assert_ne!(output.buffer_index(), input.buffer_index());
    }

    // What the block was given: two jobs, both done, each naming its bitstream and its picture.
    {
        let jobs = sim::JOBS.lock().unwrap();
        assert_eq!(jobs.len(), 2);
        for job in jobs.iter() {
            assert_eq!(job.outcome, JobStatus::Done);
            assert!(job.start_code);
            assert_eq!(job.refs.len(), 2);
            assert_eq!(job.items.len(), 8, "setup, stamp and six controls");
        }
        let setup = &jobs[0].items[0].1;
        assert_eq!(setup.len(), 20);
        assert_eq!(&setup[4..8], &320u32.to_le_bytes());
        assert_eq!(&setup[8..12], &240u32.to_le_bytes());
    }

    let mut block = client
        .open::<dyn CodecBlock>(&device.name)
        .expect("codec contract");
    let counters = block.counters().expect("counters");
    assert_eq!(
        (counters.jobs, counters.refused, counters.failed),
        (2, 0, 0)
    );

    for counter in copy_counters() {
        assert_eq!(counter.copies(), 0, "{} copied", counter.site());
    }

    drop(block);
    drop(decoder);
    drop(pool);
    drop(client);
    service.stop();
}
