//! The PiSP's two ends as the processing loop sees them: the back end as its [`FrameIsp`]
//! (whole frames, or a sliced frame's rest) and the front end as its [`InlineIsp`].

use std::time::{Duration, Instant};

use styx_algo::Statistics;
use styx_pisp::device::{BackEndStream, BeJob, QueuedJob, profile};
use styx_pisp::fe::FrontEnd;
use styx_pisp::uapi::RawStatistics;

use crate::controller::SensorValues;
use crate::error::Result;
use crate::isp::IspSettings;
use crate::pisp_be::BeConfigBuilder;
use crate::process::{FrameIsp, InlineIsp};
use crate::stats;

/// The back end as the loop's [`FrameIsp`]: the config patched for the frame's settings and
/// exposure, the job queued with the front end's raw buffer as input, then waited for.
pub(super) struct BackEnd<'a> {
    pub(super) dev: &'a mut BackEndStream,
    pub(super) builder: &'a mut BeConfigBuilder,
    pub(super) timeout: Duration,
    /// Updating the config (and its tiles, when they change).
    pub(super) prepare: Duration,
    /// Frames split into two jobs, the first started at the line event.
    pub(super) slicing: Option<&'a mut super::pisp_slices::Slicing>,
}

impl FrameIsp for BackEnd<'_> {
    type Input<'a> = u32;
    type Job = QueuedJob;
    type Output = BeJob;

    fn submit(
        &mut self,
        index: u32,
        settings: &IspSettings,
        values: &SensorValues,
        _statistics: bool,
    ) -> Result<QueuedJob> {
        if let Some(s) = self.slicing.as_deref_mut()
            && let Some(job) = s.submit_rest(index, settings, self.dev, self.timeout)?
        {
            self.prepare = Duration::ZERO;
            return Ok(job);
        }
        let t = Instant::now();
        // Temporal denoise scales its average by the frame's light: exposure, gain and the
        // flicker deflicker predicts.
        let exposure = values.exposure.as_secs_f64() * values.analogue_gain * settings.flicker;
        profile::time("loop", "be_update", || {
            self.builder.update_frame(settings, exposure)
        })?;
        self.prepare = t.elapsed();
        Ok(self.dev.process_queued(index, self.builder.config())?)
    }

    fn finish(&mut self, job: QueuedJob) -> Result<BeJob> {
        Ok(self.dev.wait_job(job, self.timeout)?)
    }

    fn discard(&mut self, job: &BeJob) {
        self.dev.release(job);
    }
}

/// The front end as the loop's [`InlineIsp`]: the statistics copied out at the dequeue (when
/// the algorithms run), settings queued with its next config.
pub(super) struct FeStats<'a> {
    pub(super) raw: &'a RawStatistics,
    pub(super) fe: &'a mut FrontEnd,
}

impl InlineIsp for FeStats<'_> {
    fn statistics(&mut self, out: &mut Statistics) -> bool {
        stats::from_pisp_raw(self.raw, out);
        true
    }

    fn set_settings(&mut self, settings: &IspSettings) {
        settings.apply_fe(self.fe);
    }
}
