//! The job path on the simulated backend: `lemnosd` runs in this process with a simulated
//! memory-fed `camera.isp/1` (`styx_sim::job_isp`) reporting the K230D ISP's register-write
//! layout, and `styx_k230::Isp` encodes each frame's register writes into a configuration buffer
//! it maps writable, posts the job, and decodes the statistics that come back for it.
//!
//! The path runs twice: on streams of shared memory, then of dma-bufs (`memory = "dma-buf"`). The
//! second needs a dma-heap this user may allocate from (see `styx_frame::dma_case` for a host without
//! one). One test, because the simulated ISP's log is process-wide.

use std::path::PathBuf;
use std::time::Duration;

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_aaa::Statistics;
use styx_k230::{Isp, Params, STATISTICS_BYTES, params};
use styx_lemnos::Client;
use styx_lemnos::k230isp::LAYOUT_WRITES;
use styx_settings::IspSettings;
use styx_sim::job_isp::{self, SimJobIsp};
use styx_sim::memory::{Memory, dma_buf_case};
use styx_soft::CfaPattern;

const JOBS: u64 = 12;
const FRAME: (u32, u32) = (640, 480);
/// The configuration buffer: room for the longest write list the layout takes.
const CONFIG_BYTES: u32 = 16_384;

fn start(memory: Memory) -> (Service, PathBuf) {
    let dir = std::env::temp_dir()
        .join(format!("styx-k230-sim-{}", std::process::id()))
        .join(format!("{memory:?}"));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("sock");
    let board = job_isp::board(job_isp::Board {
        config_layout: LAYOUT_WRITES,
        config_bytes: CONFIG_BYTES,
        statistics_layout: 0,
        statistics_bytes: STATISTICS_BYTES as u32,
        config_buffers: 2,
        config_buffer_bytes: CONFIG_BYTES,
        statistics_buffers: 4,
        memory,
    });
    let service = Service::start(Config {
        socket: socket.clone(),
        name: "styx-k230-test".to_string(),
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

/// Frame `n`'s settings: the white balance moves, so every job's writes differ.
fn settings(n: u64) -> IspSettings {
    IspSettings {
        wb: [1.0 + n as f64 / 8.0, 1.0, 2.0 - n as f64 / 16.0],
        ..IspSettings::neutral(0.0)
    }
}

fn job_path(memory: Memory) {
    job_isp::clear_seen();
    let (service, socket) = start(memory);
    let client = Client::connect(&socket, "styx-k230-test").expect("connect");
    let device = client
        .list()
        .expect("list")
        .into_iter()
        .find(|d| d.name == "isp0")
        .expect("the simulated ISP");
    let mut isp = Isp::open(&client, &device, Duration::from_secs(5)).expect("open");

    let mut posted = Vec::new();
    let mut p = Params::new();
    let mut stats = Statistics::default();
    for n in 0..JOBS {
        p = params(p, &settings(n), CfaPattern::Rggb, FRAME);
        let identity = isp.post(&p).expect("post");
        let mut bytes = vec![0u8; p.encoded_len().expect("length")];
        p.encode(&mut bytes).expect("encode");
        posted.push((identity, bytes));
        isp.statistics(identity, FRAME, &mut stats)
            .expect("the job's statistics");
    }

    // What the simulated ISP read: each job's writes, as the session encoded them in its buffer.
    let seen = job_isp::seen();
    assert_eq!(seen.len(), JOBS as usize, "{memory:?}: every job read once");
    for (s, (identity, bytes)) in seen.iter().zip(&posted) {
        assert_eq!(s.identity, *identity);
        assert_eq!(s.layout, LAYOUT_WRITES);
        assert!(
            s.statistics,
            "{memory:?}: job {identity} statistics published"
        );
        assert!(
            s.config.get(..bytes.len()) == Some(&bytes[..]),
            "{memory:?}: job {identity} read the writes the session encoded"
        );
    }
    println!(
        "k230 job path ({memory:?}): {JOBS} jobs, {} bytes of writes each encoded in place and read back",
        posted[0].1.len()
    );
    drop(isp);
    drop(client);
    service.stop();
}

#[test]
fn writes_encoded_in_place_reach_the_simulated_isp_on_shared_memory_and_dma_bufs() {
    job_path(Memory::Shared);
    if dma_buf_case("K230 ISP jobs") {
        job_path(Memory::DmaBuf);
    }
}
