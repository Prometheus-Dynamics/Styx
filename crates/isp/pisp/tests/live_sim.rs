//! The live job path on the simulated backend: `lemnosd` runs in this process with a simulated
//! `camera.isp/1` + `isp.pisp/1` (see `sim/mod.rs`), and `styx_pisp::LiveIsp` drives it through the
//! real client and streams.
//!
//! One test runs every case in order: the streams' copies are counted process-wide, and the
//! simulated ISP's log and gate are shared, so the cases share one service.

mod sim;

use std::path::PathBuf;
use std::time::Duration;

use lemnos_contract_camera_isp::NO_BUFFER;
use lemnos_contract_isp_pisp::LAYOUT_BACK_END;
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_format::{ColorRange, ColorSpace, FourCc, MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FrameLease, FramePool, copy_counters, packed_layouts};
use styx_lemnos::{Client, DeviceEntry};
use styx_pisp::pisp::format::{compute_stride_align, formats};
use styx_pisp::pisp::uapi::{BayerOrder, BeTilesConfig, ImageFormatConfig, Wire};
use styx_pisp::{BackEndBuilder, Geometry, IspSettings, LiveIsp, LiveJob, Output, PispError};

/// Frames the first case posts, and the frames the busy case adds.
const FRAMES: u64 = 12;
const BUSY_FRAMES: u64 = 3;
const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;
const COPY_SITE: &str = "styx-frame: FrameLease::copy_into";

fn socket_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-pisp-sim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start(text: &str) -> (Service, PathBuf) {
    let socket = socket_dir().join("sock");
    let config = Config {
        socket: socket.clone(),
        name: "styx-pisp-test".to_string(),
        board: parse(text).expect("board"),
        drivers: vec![entry::<sim::SimPisp>()],
        backend: Box::new(SimBoard::new()),
        options: Options {
            call_deadline: Duration::from_millis(500),
            ..Options::default()
        },
    };
    (Service::start(config).expect("start lemnosd"), socket)
}

/// The input as the front end gives it: 16-bit Bayer at the input's stride.
fn input_image() -> ImageFormatConfig {
    let mut f = ImageFormatConfig {
        width: WIDTH as u16,
        height: HEIGHT as u16,
        format: formats::BAYER16,
        stride: 0,
        stride2: 0,
    };
    compute_stride_align(&mut f, 16);
    f
}

fn input_len() -> usize {
    input_image().stride as usize * HEIGHT as usize
}

fn geometry() -> Geometry {
    let output = |code, w, h, color, align| Output {
        media: MediaFormat::new(
            code,
            Resolution::new(w, h).expect("non-empty"),
            color,
            ColorRange::Full,
        ),
        stride_align: align,
    };
    Geometry {
        input: input_image(),
        bayer: BayerOrder::Rggb,
        outputs: [
            output(FourCc::NV12, WIDTH, HEIGHT, ColorSpace::Bt601, 64),
            output(FourCc::RGB3, 640, 400, ColorSpace::Srgb, 64),
        ],
        tdn: Some(input_image()),
        slice_at: None,
    }
}

/// The recording's settings and exposures, frame by frame, from the recording's first frame on.
fn recorded_frames(n: usize) -> Vec<(IspSettings, f64)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../algo/controller/tests/data/dev-expected.jsonl");
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(path)
        .expect("the controller recording")
        .lines()
        .map(|l| serde_json::from_str(l).expect("a JSON line"))
        .collect();
    let exposure = |sensor: &serde_json::Value| {
        let e = &sensor["exposure"];
        (e["secs"].as_f64().unwrap() + e["nanos"].as_f64().unwrap() / 1e9)
            * sensor["analogue_gain"].as_f64().unwrap()
    };
    let mut expo = exposure(&lines[0]["start"]["sensor"]);
    let mut out = Vec::new();
    for line in lines[1..].iter().take(n) {
        if !line["sensor"].is_null() {
            expo = exposure(&line["sensor"]);
        }
        let isp: IspSettings = serde_json::from_value(line["isp"].clone()).expect("settings");
        let e = expo * isp.flicker;
        out.push((isp, e));
    }
    assert_eq!(out.len(), n, "the recording has the frames asked for");
    out
}

/// A raw frame of the input's size, filled with a pattern of `identity`.
fn raw_frame(pool: &FramePool<()>, identity: u64) -> FrameLease<()> {
    let format = MediaFormat::with_defaults(
        FourCc::RG16,
        Resolution::new(WIDTH, HEIGHT).expect("non-empty"),
    );
    let layouts = packed_layouts(FourCc::RG16, WIDTH, HEIGHT, Some(16), None).expect("layout");
    let entry = FrameEntry::new(identity, identity * 33_000_000, format, layouts);
    let mut lease = pool.acquire(entry).expect("a free raw buffer");
    {
        let mut planes = lease.write().expect("writable");
        for (i, b) in planes.bytes().iter_mut().enumerate() {
            *b = (i as u64).wrapping_mul(31).wrapping_add(identity * 7) as u8;
        }
    }
    lease
}

/// FNV-1a of the frame as the input stream holds it: its rows at the input's stride.
fn raw_hash(lease: &FrameLease<()>) -> u64 {
    let read = lease.read().expect("readable");
    sim::fnv1a(&read.bytes()[..input_len()])
}

fn config_bytes(c: &BeTilesConfig) -> Vec<u8> {
    let mut out = vec![0u8; BeTilesConfig::SIZE];
    c.write_to(&mut out).expect("the config fits");
    out
}

/// Whether a job's two outputs carry the fill the simulated ISP gives the job's identity, over
/// each output's own span.
fn outputs_carry_their_job(job: &LiveJob, expect: &BackEndBuilder) -> bool {
    job.outputs.iter().enumerate().all(|(k, lease)| {
        let span = styx_pisp::back_end::image_layouts(&expect.image(k))
            .expect("layout")
            .span()
            .expect("span");
        let read = lease.read().expect("output readable");
        read.bytes()[..span]
            .iter()
            .all(|&b| b == sim::fill_for(job.identity, k))
    })
}

/// Posts `identity` until it is accepted: a refusal waits for a completion, which is checked and
/// given back (its outputs are dropped) and noted in `done`. Returns how many refusals were made.
fn post_frame(
    live: &mut LiveIsp,
    identity: u64,
    settings: &IspSettings,
    exposure: f64,
    raw: &FrameLease<()>,
    expect: &BackEndBuilder,
    done: &mut Vec<u64>,
) -> usize {
    let mut refused = 0;
    loop {
        match live.post(identity, settings, exposure, raw) {
            Ok(()) => return refused,
            Err(PispError::Busy(why)) => {
                assert!(
                    live.in_flight() > 0,
                    "refused with nothing in flight: {why}"
                );
                refused += 1;
                let job = live.wait().expect("wait").expect("a completion in time");
                done.push(check(job, expect));
            }
            Err(e) => panic!("post {identity}: {e}"),
        }
    }
}

/// The identity of a completed job whose two outputs carry their own fill; the job's outputs are
/// given back by dropping it.
fn check(job: LiveJob, expect: &BackEndBuilder) -> u64 {
    assert!(
        outputs_carry_their_job(&job, expect),
        "job {} outputs",
        job.identity
    );
    job.identity
}

fn copies() -> (usize, usize) {
    copy_counters()
        .iter()
        .find(|c| c.site() == COPY_SITE)
        .map_or((0, 0), |c| (c.copies(), c.bytes()))
}

#[test]
fn live_path_on_the_simulated_pisp() {
    let (service, socket) = start(&sim::board());
    let client = Client::connect(&socket, "styx-pisp-test").expect("connect");
    let devices: Vec<DeviceEntry> = client.list().expect("list");
    let device = devices
        .iter()
        .find(|d| d.name == "isp0")
        .expect("the simulated ISP")
        .clone();
    let geometry = geometry();
    let mut live =
        LiveIsp::open(&client, &device, &geometry, Duration::from_secs(10)).expect("open");

    // The test's own builder, run over the same frames in the same order: the configurations the
    // jobs must name.
    let mut expect = BackEndBuilder::new(&geometry).expect("builder");
    let total = FRAMES + BUSY_FRAMES;
    let frames = recorded_frames(total as usize);
    let pool: FramePool<()> =
        FramePool::host(4, BufferSize::new(input_len(), 0)).expect("raw pool");
    let (copies_before, bytes_before) = copies();

    // Case 1: every frame posted, completions in order, both outputs as leases.
    let mut done: Vec<u64> = Vec::new();
    let mut refusals = 0;
    for (k, (settings, exposure)) in frames.iter().take(FRAMES as usize).enumerate() {
        let identity = k as u64;
        expect.update(settings, *exposure).expect("expected config");
        let raw = raw_frame(&pool, identity);
        sim::expect(
            identity,
            sim::Expect {
                config: config_bytes(expect.config()),
                input_hash: raw_hash(&raw),
                input_len: input_len(),
            },
        );
        refusals += post_frame(
            &mut live, identity, settings, *exposure, &raw, &expect, &mut done,
        );
    }
    assert!(refusals > 0, "a second frame in flight waits for the first");
    while (done.len() as u64) < FRAMES {
        let job = live.wait().expect("wait").expect("a completion in time");
        done.push(check(job, &expect));
    }
    // Job-driven streams never replace an unread result: nothing was skipped. Every refused post
    // was counted by the session and sent nothing.
    assert_eq!(live.skipped(), 0, "a job-driven result was replaced");
    assert_eq!(live.refused(), refusals as u64, "refusals counted");
    assert_eq!(
        done,
        (0..FRAMES).collect::<Vec<_>>(),
        "completions in order"
    );
    done.clear();

    // Case 2: a frame posted while both config buffers are in flight is refused, nothing is sent
    // and nothing is dropped; it goes once a completion is taken.
    sim::set_gate(true);
    let ran_before = sim::ran().len();
    let mut busy_frames = Vec::new();
    for (k, (settings, exposure)) in frames.iter().skip(FRAMES as usize).enumerate() {
        let identity = FRAMES + k as u64;
        expect.update(settings, *exposure).expect("expected config");
        let raw = raw_frame(&pool, identity);
        sim::expect(
            identity,
            sim::Expect {
                config: config_bytes(expect.config()),
                input_hash: raw_hash(&raw),
                input_len: input_len(),
            },
        );
        busy_frames.push((identity, settings.clone(), *exposure, raw));
    }
    let (id0, s0, e0, r0) = &busy_frames[0];
    let (id1, s1, e1, r1) = &busy_frames[1];
    let (id2, s2, e2, r2) = &busy_frames[2];
    live.post(*id0, s0, *e0, r0).expect("first in flight");
    live.post(*id1, s1, *e1, r1).expect("second in flight");
    assert_eq!(live.in_flight(), 2);
    let copies_mid = copies().0;
    match live.post(*id2, s2, *e2, r2) {
        Err(PispError::Busy(why)) => {
            assert!(why.contains("config"), "refused for the config: {why}")
        }
        other => panic!("a third frame in flight must be refused, got {other:?}"),
    }
    assert_eq!(live.in_flight(), 2, "the refused frame is not in flight");
    assert_eq!(copies().0, copies_mid, "a refused frame copies nothing");
    sim::set_gate(false);
    let mut busy_done = Vec::new();
    while busy_done.len() < 2 {
        let job = live.wait().expect("wait").expect("a completion in time");
        busy_done.push(check(job, &expect));
    }
    live.post(*id2, s2, *e2, r2)
        .expect("the refused frame goes once one completed");
    let job = live.wait().expect("wait").expect("completion");
    busy_done.push(check(job, &expect));
    assert_eq!(
        busy_done,
        vec![*id0, *id1, *id2],
        "the busy case completes in order"
    );
    assert_eq!(
        sim::ran().len(),
        ran_before + 3,
        "each frame ran once: the refused one not at all"
    );
    drop(busy_frames);

    // What the simulated ISP ran, for every frame: the configuration and the input it named are
    // the ones the test made, the two configuration buffers alternate, and both outputs were made.
    let ran = sim::ran();
    assert_eq!(ran.len() as u64, total, "every frame ran exactly once");
    for (n, r) in ran.iter().enumerate() {
        assert_eq!(r.identity, n as u64, "jobs ran in posting order");
        assert_eq!(r.layout, LAYOUT_BACK_END);
        assert!(
            r.config_ok,
            "job {} named the configuration the builder made",
            r.identity
        );
        assert!(
            r.input_ok,
            "job {} read the frame that was copied in",
            r.identity
        );
        assert_eq!(r.config_buffer, (n % 2) as u32, "config buffers alternate");
        assert!(
            r.outputs.iter().all(|&o| o != NO_BUFFER),
            "both outputs named"
        );
    }

    // Copies: the input write is the only one, and it is one per frame posted.
    let (copies_after, bytes_after) = copies();
    assert_eq!(
        copies_after - copies_before,
        total as usize,
        "one input copy per frame"
    );
    assert_eq!(
        bytes_after - bytes_before,
        total as usize * input_len(),
        "the copied bytes"
    );
    assert_eq!(live.input_copies(), total as usize);

    eprintln!(
        "live path proof: jobs run {}; refused while both config buffers were in flight: {} (case 1), \
         {} (case 2); input copies {} = {} bytes; output copies 0 (styx-frame copy counter: {} copies, {} bytes); \
         config bytes written in place {} per job",
        ran.len(),
        refusals,
        1,
        live.input_copies(),
        bytes_after - bytes_before,
        copies_after - copies_before,
        bytes_after - bytes_before,
        BeTilesConfig::SIZE
    );
    live.stop().expect("stop the ISP");
    drop(client);
    service.stop();
}
