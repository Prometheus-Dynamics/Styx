//! Extra back end passes over each frame's raw input (see [`crate::pisp_passes`]): after the
//! main job, while the front end still holds the raw buffer, one job per pass (queued back to
//! back), so every region comes from the same raw frame, with the same settings, as the frame
//! it goes with.

use std::time::Duration;

use styx_pisp::device::{BackEndStream, DeviceError, QUEUED_JOBS, QueuedJob};

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

/// Runs `passes` over raw buffer `input` after `main`'s job: up to `jobs` of them (at most
/// [`QUEUED_JOBS`]) are queued back to back, the input buffer in each, and waited for in
/// order, so the back end goes from one to the next without a round trip through this thread
/// between them; `jobs` 1 runs them one at a time. A pass without a free output buffer is
/// skipped this frame.
pub(super) fn run(
    dev: &mut BackEndStream,
    passes: &mut PassConfigs,
    main: &BeConfigBuilder,
    input: u32,
    jobs: usize,
    timeout: Duration,
) -> Result<Vec<Option<PassOutput>>> {
    let jobs = jobs.clamp(1, QUEUED_JOBS);
    let mut out: Vec<Option<PassOutput>> = Vec::with_capacity(passes.len());
    let mut queued = Ring::default();
    let mut failed: Option<DeviceError> = None;
    for k in 0..passes.len() {
        out.push(None);
        let Some(spec) = passes.spec(k) else { continue };
        let cfg = match passes.config(k, main) {
            Ok(Some(cfg)) => cfg,
            Ok(None) => continue,
            Err(e) => {
                failed = Some(DeviceError::Setup(e.to_string()));
                break;
            }
        };
        if queued.len() == jobs
            && let Err(e) = finish(dev, &mut queued, &mut out, timeout)
        {
            failed = Some(e);
            break;
        }
        match dev.process_queued(input, cfg) {
            Ok(job) => queued.push((k, spec, job)),
            Err(DeviceError::OutputsHeld(_)) => {}
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    // Every queued job is waited for, even after a failure: its buffers are the device's
    // until then.
    while !queued.is_empty() {
        if let Err(e) = finish(dev, &mut queued, &mut out, timeout) {
            failed.get_or_insert(e);
        }
    }
    if let Some(e) = failed {
        for p in out.iter().flatten() {
            dev.release_output(p.output, p.index);
        }
        return Err(e.into());
    }
    Ok(out)
}

/// Queued pass jobs, oldest first: (pass slot, spec, job). On the stack: no allocation per
/// frame.
#[derive(Default)]
struct Ring {
    jobs: [Option<(usize, PassSpec, QueuedJob)>; QUEUED_JOBS],
    head: usize,
    len: usize,
}

impl Ring {
    fn len(&self) -> usize {
        self.len
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, job: (usize, PassSpec, QueuedJob)) {
        debug_assert!(self.len < QUEUED_JOBS);
        self.jobs[(self.head + self.len) % QUEUED_JOBS] = Some(job);
        self.len += 1;
    }

    fn pop_front(&mut self) -> Option<(usize, PassSpec, QueuedJob)> {
        let job = self.jobs[self.head].take()?;
        self.head = (self.head + 1) % QUEUED_JOBS;
        self.len -= 1;
        Some(job)
    }
}

/// Waits for the oldest queued pass job and puts its output in its slot.
fn finish(
    dev: &mut BackEndStream,
    queued: &mut Ring,
    out: &mut [Option<PassOutput>],
    timeout: Duration,
) -> std::result::Result<(), DeviceError> {
    let Some((k, spec, job)) = queued.pop_front() else {
        return Ok(());
    };
    let job = dev.wait_job(job, timeout)?;
    out[k] = job.outputs[spec.output].map(|index| PassOutput {
        output: spec.output,
        index,
        elapsed: job.elapsed,
        spec,
    });
    Ok(())
}
