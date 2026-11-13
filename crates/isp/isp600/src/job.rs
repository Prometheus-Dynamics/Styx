//! The `camera.isp/1` job for one frame: a load image in, the statistics and image out.

use lemnos_contract_camera_isp::{IspJob, MAX_OUTPUTS, NO_BUFFER};
use lemnos_contract_isp_isp600::LAYOUT_LOAD;
use lemnos_core::{Encode, Writer};
use styx_lemnos::JobStream;

/// The most bytes one encoded [`IspJob`] takes (its fields are 8 + 4 * 7).
pub const JOB_BYTES: usize = 64;

/// The job for `identity`: `config_buffer` holds the load image, `output` is the image buffer on
/// the first output's stream, or `None` to skip the image this frame.
#[must_use]
pub fn job(identity: u64, config_buffer: u32, output: Option<u32>) -> IspJob {
    let mut outputs = [NO_BUFFER; MAX_OUTPUTS];
    if let Some(buffer) = output {
        outputs[0] = buffer;
    }
    IspJob {
        identity,
        layout: LAYOUT_LOAD,
        config_buffer,
        input_buffer: NO_BUFFER,
        outputs,
    }
}

/// Why a job was not posted.
#[derive(Debug)]
pub enum PostError {
    /// The job did not encode.
    Encode(lemnos_core::Status),
    /// The stream refused the post.
    Stream(styx_lemnos::Error),
}

impl std::fmt::Display for PostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encode(status) => write!(f, "the job did not encode: {status:?}"),
            Self::Stream(error) => write!(f, "the job stream refused the post: {error}"),
        }
    }
}

impl std::error::Error for PostError {}

/// The job as the contract's codec encodes it: its bytes, and how many of them are the job. No
/// allocation.
pub fn encode(job: &IspJob) -> Result<([u8; JOB_BYTES], usize), PostError> {
    let mut bytes = [0u8; JOB_BYTES];
    let mut writer = Writer::new(&mut bytes);
    job.encode(&mut writer).map_err(PostError::Encode)?;
    let used = writer.len();
    Ok((bytes, used))
}

/// Posts `job` on the config stream. `Ok(false)`: the queue is full and nothing was posted.
pub fn post<M: Default + Send + Sync>(
    stream: &mut JobStream<M>,
    job: &IspJob,
) -> Result<bool, PostError> {
    let (bytes, used) = encode(job)?;
    stream.post(&bytes[..used]).map_err(PostError::Stream)
}
