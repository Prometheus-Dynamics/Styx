//! A simulated memory-fed `camera.isp/1` device on Lemnos's simulated backend: a configuration
//! stream the consumer fills and posts jobs on, and a statistics stream it fills in return. It is
//! what an ISP whose jobs carry only parameters looks like from Styx (the jobs name a config buffer
//! and no input), for a test of a session that writes those parameters into a mapped buffer.
//!
//! Each tick it reads every job the ring holds with the contract's codec ([`IspJob::decode`]),
//! reads the configuration buffer the job names into its own memory, logs both ([`seen`]), and
//! publishes one statistics frame for the job, numbered by the job's identity and filled with
//! [`statistics_fill`]. Nothing about the parameters is interpreted.
//!
//! The board's `config` gives what the device reports, so a session that checks them finds the
//! layouts it expects:
//! - `config_layout`, `config_bytes`: the one configuration layout ([`CameraIsp::config_layout`]).
//!   A job naming another layout is dropped and counted.
//! - `statistics_layout`, `statistics_bytes`: [`CameraIsp::info`]'s, and the length of each
//!   statistics frame.
//!
//! Register it with `entry::<SimJobIsp>()`; [`board`] gives a board with one.

use std::sync::{Mutex, PoisonError};

use lemnos_contract_camera_isp::{
    CameraIsp, ConfigLayout, Counters, IspInfo, IspInput, IspJob, IspOutput, OutputLayout,
};
use lemnos_core::{
    Config, ConfigError, Decode, Key, Kind, Lifecycle, Reader, Result, Status, StreamHandle, driver,
};
use lemnos_hal::blocks::StreamFrame;

use crate::memory::Memory;

const KEYS: &[Key] = &[
    Key::required("config_layout", Kind::Int),
    Key::required("config_bytes", Kind::Int),
    Key::required("statistics_layout", Kind::Int),
    Key::required("statistics_bytes", Kind::Int),
];

/// The most jobs one tick reads.
const JOBS_PER_TICK: usize = 4;

/// One job the simulated ISP read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub identity: u64,
    pub layout: u32,
    pub config_buffer: u32,
    /// The configuration buffer's first `config_bytes` bytes, as the ISP read them.
    pub config: Vec<u8>,
    /// A statistics buffer was free for the job: its frame is published right after the log entry.
    pub statistics: bool,
}

static SEEN: Mutex<Vec<Seen>> = Mutex::new(Vec::new());

/// The jobs read so far, in order, by every simulated job ISP in the process.
pub fn seen() -> Vec<Seen> {
    SEEN.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Forgets the jobs read so far.
pub fn clear_seen() {
    SEEN.lock().unwrap_or_else(PoisonError::into_inner).clear();
}

/// The byte every statistics frame of job `identity` is filled with.
#[must_use]
pub fn statistics_fill(identity: u64) -> u8 {
    (identity as u8).wrapping_mul(13).wrapping_add(5)
}

#[driver("sim-job-isp", provides = [CameraIsp])]
pub struct SimJobIsp {
    #[needs(job)]
    config: StreamHandle,
    #[needs(free)]
    statistics: StreamHandle,
    layout: ConfigLayout,
    statistics_layout: u32,
    statistics_bytes: u32,
    input: Option<IspInput>,
    running: bool,
    counters: Counters,
    sequence: u64,
}

fn int(cfg: &Config<'_>, key: &str) -> Result<u32> {
    cfg.int(key)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or(Status::INVALID_ARGUMENT)
}

impl SimJobIsp {
    /// Reads one job, its configuration, and publishes its statistics.
    fn run_job(&mut self, bytes: &[u8], now_ns: u64) -> Result<()> {
        let Ok(job) = IspJob::decode(&mut Reader::new(bytes)) else {
            self.counters.dropped += 1;
            return Ok(());
        };
        if job.layout != self.layout.layout {
            self.counters.dropped += 1;
            return Ok(());
        }
        let mut config = vec![0u8; self.layout.bytes as usize];
        if !self
            .config
            .read(job.config_buffer, 0, &mut config)
            .map_err(|_| Status::IO)?
        {
            config.clear();
        }
        let mut seen = Seen {
            identity: job.identity,
            layout: job.layout,
            config_buffer: job.config_buffer,
            config,
            statistics: false,
        };
        // The job is logged before its statistics go out: a test that has the statistics finds the
        // job in the log.
        let buffer = self.statistics.next_buffer().map_err(|_| Status::IO)?;
        seen.statistics = buffer.is_some();
        SEEN.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(seen);
        match buffer {
            Some(buffer) => {
                let fill = vec![statistics_fill(job.identity); self.statistics_bytes as usize];
                self.statistics
                    .write(buffer, 0, &fill)
                    .map_err(|_| Status::IO)?;
                self.sequence += 1;
                self.statistics
                    .publish(StreamFrame {
                        buffer,
                        bytes: self.statistics_bytes,
                        sequence: self.sequence,
                        identity: job.identity,
                        timestamp_ns: now_ns,
                        ..Default::default()
                    })
                    .map_err(|_| Status::IO)?;
            }
            None => self.counters.dropped += 1,
        }
        self.counters.jobs += 1;
        Ok(())
    }
}

impl Lifecycle for SimJobIsp {
    type Needs = SimJobIspNeeds;
    const TICKS: bool = true;

    fn check_config<'a>(cfg: &Config<'a>) -> core::result::Result<(), ConfigError<'a>> {
        cfg.check_keys(KEYS)
    }

    fn start(needs: SimJobIspNeeds, cfg: &Config<'_>) -> Result<Self> {
        Ok(Self {
            config: needs.config,
            statistics: needs.statistics,
            layout: ConfigLayout {
                layout: int(cfg, "config_layout")?,
                bytes: int(cfg, "config_bytes")?,
            },
            statistics_layout: int(cfg, "statistics_layout")?,
            statistics_bytes: int(cfg, "statistics_bytes")?,
            input: None,
            running: false,
            counters: Counters::default(),
            sequence: 0,
        })
    }

    fn tick(&mut self, now_ns: u64) -> Option<u64> {
        // The buffers the consumer gave back, so each job's buffer is the producer's to read.
        while let Ok(Some(_)) = self.config.next_buffer() {}
        let mut encoded = [0u8; 128];
        for _ in 0..JOBS_PER_TICK {
            let Ok(Some(len)) = self.config.next_job(&mut encoded) else {
                break;
            };
            let bytes = encoded[..len].to_vec();
            if self.run_job(&bytes, now_ns).is_err() {
                self.counters.errors += 1;
            }
        }
        Some(now_ns.saturating_add(500_000))
    }
}

impl CameraIsp for SimJobIsp {
    fn input_layout(&mut self) -> Result<Option<OutputLayout>> {
        Err(Status::NOT_SUPPORTED)
    }

    fn output_layout(&mut self, _output: u8) -> Result<Option<OutputLayout>> {
        Err(Status::NOT_SUPPORTED)
    }

    fn info(&mut self) -> Result<IspInfo> {
        Ok(IspInfo {
            max_outputs: 0,
            max_width: 4096,
            max_height: 4096,
            config_layouts: 1,
            statistics_layout: self.statistics_layout,
            statistics_bytes: self.statistics_bytes,
            ..Default::default()
        })
    }

    fn config_layout(&mut self, index: u8) -> Result<ConfigLayout> {
        if index != 0 {
            return Err(Status::NOT_FOUND);
        }
        Ok(self.layout)
    }

    fn set_input(&mut self, input: IspInput) -> Result<()> {
        if self.running {
            return Err(Status::BUSY);
        }
        self.input = Some(input);
        Ok(())
    }

    fn set_output(&mut self, _output: u8, _image: IspOutput) -> Result<OutputLayout> {
        Err(Status::NOT_FOUND)
    }

    fn clear_output(&mut self, _output: u8) -> Result<()> {
        Err(Status::NOT_FOUND)
    }

    fn config_stream(&mut self, into: &mut [u8]) -> Result<usize> {
        name(b"config", into)
    }

    fn statistics_stream(&mut self, into: &mut [u8]) -> Result<usize> {
        name(b"statistics", into)
    }

    fn input_stream(&mut self, _into: &mut [u8]) -> Result<usize> {
        Err(Status::NOT_SUPPORTED)
    }

    fn output_stream(&mut self, _output: u8, _into: &mut [u8]) -> Result<usize> {
        Err(Status::NOT_FOUND)
    }

    fn start(&mut self) -> Result<()> {
        self.running = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.running = false;
        Ok(())
    }

    fn counters(&mut self) -> Result<Counters> {
        Ok(self.counters)
    }
}

impl std::fmt::Debug for SimJobIsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimJobIsp").finish_non_exhaustive()
    }
}

fn name(stream: &[u8], into: &mut [u8]) -> Result<usize> {
    into.get_mut(..stream.len())
        .ok_or(Status::BUFFER_TOO_SMALL)?
        .copy_from_slice(stream);
    Ok(stream.len())
}

/// What [`board`] gives the device.
#[derive(Clone, Copy, Debug)]
pub struct Board {
    pub config_layout: u32,
    pub config_bytes: u32,
    pub statistics_layout: u32,
    pub statistics_bytes: u32,
    /// The configuration stream's buffers: each holds one job's parameters.
    pub config_buffers: u32,
    pub config_buffer_bytes: u32,
    pub statistics_buffers: u32,
    /// What both streams are made of.
    pub memory: Memory,
}

/// A board with one simulated job ISP, `isp0`.
#[must_use]
pub fn board(b: Board) -> String {
    let m = b.memory.line();
    format!(
        "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-job-isp\"\n\
         [[devices]]\nid = \"isp0\"\ndriver = \"sim-job-isp\"\n\
         config = {{ config_layout = {}, config_bytes = {}, statistics_layout = {}, statistics_bytes = {} }}\n\
         [devices.streams.config]\nbuffers = {}\nbuffer_bytes = {}\njob_slots = 4\njob_bytes = 128\n{m}\
         [devices.streams.statistics]\nbuffers = {}\nbuffer_bytes = {}\n{m}",
        b.config_layout,
        b.config_bytes,
        b.statistics_layout,
        b.statistics_bytes,
        b.config_buffers,
        b.config_buffer_bytes,
        b.statistics_buffers,
        b.statistics_bytes,
    )
}
