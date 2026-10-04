//! Extra back end passes over each frame's raw input (see [`crate::pisp_passes`]): after the
//! main job, while the front end still holds the raw buffer, one job per pass, so every
//! region comes from the same raw frame, with the same settings, as the frame it goes with.

use std::time::Duration;

use styx_pisp::device::{BackEndStream, DeviceError};

use super::{PassOutput, PispPipeline};
use crate::error::Result;
use crate::pisp_be::BeConfigBuilder;
use crate::pisp_passes::{PassConfigs, PassSpec, PassTdn};

impl PispPipeline {
    /// Runs pass `spec` in slot `k` over every frame from the next one on (`None`: no pass
    /// in the slot). Its output buffer comes with the frame ([`super::PispFrame::passes`]).
    /// Fails, changing nothing, when the back end cannot make it (a region outside the frame,
    /// or larger than the output's buffers).
    pub fn set_pass(&mut self, k: usize, spec: Option<PassSpec>) -> Result<()> {
        self.passes.set(k, spec, &self.be)
    }

    /// Slot `k`'s pass.
    pub fn pass(&self, k: usize) -> Option<PassSpec> {
        self.passes.spec(k)
    }

    /// What extra passes do with temporal denoise, from the next frame on.
    pub fn set_pass_tdn(&mut self, tdn: PassTdn) {
        self.passes.set_tdn(tdn);
    }

    /// The bytes a pass wrote (its output's buffer; the region in its top left, in the
    /// buffer's stride, its chroma plane where the buffer's height puts it). Bracket CPU
    /// reads with [`Self::sync_pass`].
    pub fn pass_output(&self, pass: &PassOutput) -> Option<&[u8]> {
        self.be_dev.as_ref()?.output_data(pass.output, pass.index)
    }

    /// [`Self::sync_output`] for a pass's buffer.
    pub fn sync_pass(&self, pass: &PassOutput, start: bool) -> Result<()> {
        match self.be_dev.as_ref() {
            Some(b) => Ok(b.sync_output(pass.output, pass.index, start)?),
            None => Ok(()),
        }
    }

    /// Gives the frame's pass buffers back (see [`Self::release`]).
    pub fn release_passes(&mut self, passes: &[Option<PassOutput>]) {
        for p in passes.iter().flatten() {
            self.release_output(p.output, p.index);
        }
    }

    /// How many times extra passes' tiles were prepared (a pass moved, or the main config was
    /// prepared again).
    pub fn pass_prepares(&self) -> u64 {
        self.passes.prepares()
    }
}

/// Runs `passes` over raw buffer `input` after `main`'s job, one job at a time (the input
/// buffer is queued once per job). A pass without a free output buffer is skipped this frame.
pub(super) fn run(
    dev: &mut BackEndStream,
    passes: &mut PassConfigs,
    main: &BeConfigBuilder,
    input: u32,
    timeout: Duration,
) -> Result<Vec<Option<PassOutput>>> {
    let mut out: Vec<Option<PassOutput>> = Vec::with_capacity(passes.len());
    for k in 0..passes.len() {
        let Some(spec) = passes.spec(k) else {
            out.push(None);
            continue;
        };
        let job = match passes.config(k, main) {
            Ok(Some(cfg)) => dev.process(input, cfg, timeout),
            Ok(None) => {
                out.push(None);
                continue;
            }
            Err(e) => Err(DeviceError::Setup(e.to_string())),
        };
        match job {
            Ok(job) => out.push(job.outputs[spec.output].map(|index| PassOutput {
                output: spec.output,
                index,
                elapsed: job.elapsed,
                spec,
            })),
            Err(DeviceError::OutputsHeld(_)) => out.push(None),
            Err(e) => {
                for p in out.iter().flatten() {
                    dev.release_output(p.output, p.index);
                }
                return Err(e.into());
            }
        }
    }
    Ok(out)
}
