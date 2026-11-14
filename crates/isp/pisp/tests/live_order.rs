//! The live path's ordering that lost a completion under load, reproduced step by step.
//!
//! A job's output buffer is given back to Lemnos by its completion, and the next job may name
//! that buffer straight away. The simulated ISP must take a returned buffer before it judges a
//! job that names it, even when it looked at the stream earlier in the same serve (the PiSP
//! driver takes returns before every job it reads). Here the ordering is put in place: the ISP
//! is held after its look, the completion is taken and the next job posted, and the ISP then
//! reads that job. Nothing here depends on timing.

mod sim;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_format::{ColorRange, ColorSpace, FourCc, MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FrameLease, FramePool, packed_layouts};
use styx_lemnos::Client;
use styx_pisp::pisp::format::{compute_stride_align, formats};
use styx_pisp::pisp::uapi::{BayerOrder, ImageFormatConfig};
use styx_pisp::{Geometry, IspSettings, LiveIsp, Output};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;

fn start() -> (Service, PathBuf) {
    let dir = std::env::temp_dir().join(format!("styx-pisp-order-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("sock");
    let config = Config {
        socket: socket.clone(),
        name: "styx-pisp-order".to_string(),
        board: parse(&sim::board()).expect("board"),
        drivers: vec![entry::<sim::SimPisp>()],
        backend: Box::new(SimBoard::new()),
        options: Options {
            call_deadline: Duration::from_millis(500),
            ..Options::default()
        },
    };
    (Service::start(config).expect("start lemnosd"), socket)
}

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
        planes.bytes().fill(identity as u8);
    }
    lease
}

/// The first recorded settings of the controller's recording: any valid frame will do here.
fn settings() -> (IspSettings, f64) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../algo/controller/tests/data/dev-expected.jsonl");
    let first: serde_json::Value = std::fs::read_to_string(path)
        .expect("the controller recording")
        .lines()
        .nth(1)
        .map(|l| serde_json::from_str(l).expect("a JSON line"))
        .expect("a frame");
    let isp: IspSettings = serde_json::from_value(first["isp"].clone()).expect("settings");
    (isp, 0.01)
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_job_naming_a_buffer_returned_after_the_isps_look_still_completes() {
    let (service, socket) = start();
    let client = Client::connect(&socket, "styx-pisp-order").expect("connect");
    let device = client
        .list()
        .expect("list")
        .into_iter()
        .find(|d| d.name == "isp0")
        .expect("the simulated ISP");
    let mut live =
        LiveIsp::open(&client, &device, &geometry(), Duration::from_secs(2)).expect("open");
    let pool: FramePool<()> =
        FramePool::host(4, BufferSize::new(input_len(), 0)).expect("raw pool");
    let (settings, exposure) = settings();

    // Job 0 runs and publishes both outputs into output buffer 0 of each stream.
    live.post(0, &settings, exposure, &raw_frame(&pool, 0))
        .expect("post job 0");
    wait_until("the ISP to run job 0", || !sim::ran().is_empty());

    // The ISP's next serve looks at its streams (job 0's output buffers are still the
    // completion's: not returned yet), then is held before it reads any job.
    sim::pause_after_take(true);
    wait_until("the ISP to hold after its look", sim::paused);

    // Taking job 0's completion gives its output buffers back to Lemnos, after the ISP's look.
    let job = live.wait().expect("wait").expect("job 0 completes");
    assert_eq!(job.identity, 0);
    drop(job);

    // Job 1 names output buffer 0 again: it is the first free one.
    live.post(1, &settings, exposure, &raw_frame(&pool, 1))
        .expect("post job 1");

    // The ISP reads job 1 in the serve it looked in before the return. It must take the return
    // before it judges the job, or the job's outputs are never made.
    sim::pause_after_take(false);
    let job = live
        .wait()
        .expect("wait for job 1")
        .expect("job 1 completes: the buffers it names were returned before it was read");
    assert_eq!(job.identity, 1);

    live.stop().expect("stop the ISP");
    drop(client);
    service.stop();
}
