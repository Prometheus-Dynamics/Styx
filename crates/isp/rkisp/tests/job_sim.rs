//! The job path on the simulated backend: `lemnosd` runs in this process with a simulated
//! memory-fed `camera.isp/1` (`styx_sim::job_isp`) reporting ISP21's layouts, and
//! `styx_rkisp::Isp` writes each frame's parameters into a configuration buffer it maps writable,
//! posts the job, and reads the statistics that come back for it.
//!
//! The path runs twice: on streams of shared memory, then of dma-bufs (`memory = "dma-buf"`). The
//! second needs a dma-heap this user may allocate from (see `styx_frame::dma_case` for a host without
//! one). One test, because the simulated ISP's log is process-wide.

use std::path::PathBuf;
use std::time::Duration;

use lemnos_contract_camera_isp::{MAX_OUTPUTS, NO_BUFFER};
use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_aaa::Statistics;
use styx_lemnos::Client;
use styx_rkisp::{Generation, Isp, Params};
use styx_settings::IspSettings;
use styx_sim::job_isp::{self, SimJobIsp};
use styx_sim::memory::{Memory, dma_buf_case};

const JOBS: u64 = 12;
const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const GENERATION: Generation = Generation::Isp21;

fn start(memory: Memory) -> (Service, PathBuf) {
    let dir = std::env::temp_dir()
        .join(format!("styx-rkisp-sim-{}", std::process::id()))
        .join(format!("{memory:?}"));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("sock");
    let config = GENERATION.config_layout();
    let statistics = GENERATION.statistics_layout();
    let board = job_isp::board(job_isp::Board {
        config_layout: config.layout,
        config_bytes: config.bytes,
        statistics_layout: statistics.layout,
        statistics_bytes: statistics.bytes,
        config_buffers: 4,
        config_buffer_bytes: config.bytes.next_multiple_of(4096),
        statistics_buffers: 4,
        memory,
    });
    let service = Service::start(Config {
        socket: socket.clone(),
        name: "styx-rkisp-test".to_string(),
        board: parse(&board).expect("board"),
        drivers: vec![entry::<SimJobIsp>()],
        backend: Box::new(SimBoard::new()),
        options: Options {
            call_deadline: Duration::from_millis(500),
            ..Options::default()
        },
    })
    .expect("start lemnosd");
    (service, socket)
}

/// Frame `n`'s settings: the white balance moves, so every job's parameters differ.
fn settings(n: u64) -> IspSettings {
    IspSettings {
        wb: [1.0 + n as f64 / 8.0, 1.0, 2.0 - n as f64 / 16.0],
        ..IspSettings::neutral(0.0)
    }
}

fn job_path(memory: Memory) {
    job_isp::clear_seen();
    let (service, socket) = start(memory);
    let client = Client::connect(&socket, "styx-rkisp-test").expect("connect");
    let device = client
        .list()
        .expect("list")
        .into_iter()
        .find(|d| d.name == "isp0")
        .expect("the simulated ISP");
    let mut isp = Isp::open(&client, &device).expect("open");
    assert_eq!(isp.generation(), GENERATION);

    let mut posted = Vec::new();
    let mut params = Params::new(GENERATION);
    let mut stats = Statistics::default();
    for n in 0..JOBS {
        params
            .apply(&settings(n), n as u32, WIDTH, HEIGHT)
            .expect("ISP21 parameters");
        let identity = isp
            .post(&params, [NO_BUFFER; MAX_OUTPUTS])
            .expect("post")
            .expect("a free config buffer: the previous job's statistics were taken");
        posted.push((identity, params.as_bytes().to_vec()));
        let got = isp
            .statistics(Duration::from_secs(5), 1, &mut stats)
            .expect("statistics")
            .expect("statistics in time");
        assert_eq!(got, identity, "{memory:?}: the statistics are the job's");
        assert!(!stats.colour.zones.is_empty(), "{memory:?}: decoded");
    }

    // What the simulated ISP read: each job's parameters, as the session wrote them in its buffer.
    let seen = job_isp::seen();
    assert_eq!(seen.len(), JOBS as usize, "{memory:?}: every job read once");
    for (s, (identity, bytes)) in seen.iter().zip(&posted) {
        assert_eq!(s.identity, *identity);
        assert_eq!(s.layout, GENERATION.config_layout().layout);
        assert!(
            s.statistics,
            "{memory:?}: job {identity} statistics published"
        );
        assert!(
            s.config == *bytes,
            "{memory:?}: job {identity} read the parameters the session wrote"
        );
    }
    println!(
        "rkisp job path ({memory:?}): {JOBS} jobs, {} parameter bytes each written in place and read back",
        posted[0].1.len()
    );
    drop(isp);
    drop(client);
    service.stop();
}

#[test]
fn parameters_written_in_place_reach_the_simulated_isp_on_shared_memory_and_dma_bufs() {
    job_path(Memory::Shared);
    if dma_buf_case("rkisp ISP jobs") {
        job_path(Memory::DmaBuf);
    }
}
