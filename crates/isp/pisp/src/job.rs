//! ISP jobs: the `camera.isp/1` job posted on the config stream, which names the configuration
//! buffer (the library's back-end tiles config, [`lemnos_contract_isp_pisp::LAYOUT_BACK_END`])
//! and the output buffers of one frame.

use lemnos_contract_camera_isp::{IspJob, MAX_OUTPUTS, NO_BUFFER};
use lemnos_core::{Encode, Writer};
use styx_lemnos::pisp::uapi::{BeTilesConfig, Wire};

use crate::PispError;

/// Bytes a job takes at most (its encoding is well under this).
pub const JOB_CAPACITY: usize = 128;

/// The back-end job for one frame: `identity` (the caller's name for the frame, carried),
/// `config` the buffer holding the tiles config, `input` the raw buffer (or `None` when the
/// frame comes straight from the receiver) and the output buffers ([`None`] skips an output).
pub fn back_end_job(
    identity: u64,
    config: u32,
    input: Option<u32>,
    outputs: [Option<u32>; MAX_OUTPUTS],
) -> IspJob {
    IspJob {
        identity,
        layout: lemnos_contract_isp_pisp::LAYOUT_BACK_END,
        config_buffer: config,
        input_buffer: input.unwrap_or(NO_BUFFER),
        outputs: outputs.map(|o| o.unwrap_or(NO_BUFFER)),
    }
}

/// Encodes `job` into `out`, returning its length.
pub fn encode_job(job: &IspJob, out: &mut [u8]) -> Result<usize, PispError> {
    let mut w = Writer::new(out);
    job.encode(&mut w)
        .map_err(|e| PispError::Job(format!("{e:?}")))?;
    Ok(w.len())
}

/// The tiles config as the bytes its buffer holds (the library's own encoding, padding zero).
pub fn config_bytes(cfg: &BeTilesConfig, out: &mut [u8]) -> Result<usize, PispError> {
    cfg.write_to(out)
        .map_err(|e| PispError::Job(format!("config buffer: {e}")))
}

/// Where jobs go: a `camera.isp/1` job stream (`styx_lemnos::JobStream`, which posts the bytes on
/// the ring). Implemented for the stream; a test sink records them.
pub trait JobSink {
    /// Posts `job`. `Ok(false)` when the ring is full.
    fn post(&mut self, job: &[u8]) -> Result<bool, PispError>;
}

impl<M: Default + Send + Sync> JobSink for styx_lemnos::JobStream<M> {
    fn post(&mut self, job: &[u8]) -> Result<bool, PispError> {
        styx_lemnos::JobStream::post(self, job).map_err(|e| PispError::Sink(format!("{e}")))
    }
}
