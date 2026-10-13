//! Processing the top of a frame while the bottom is still being read (`PispOptions::slice_line`,
//! needs Styx's rp1-cfe module). The front end raises an event once its raw output has `line`
//! rows in memory (they are final then: measured on the CM5, every row up to the line, in every
//! frame). At that event the back end runs the tiles above the line as one job on the raw
//! buffer still being written; at the end of the frame the rest of the tiles run as a second
//! job into the same output buffers. The frame comes out the same, sooner after its readout.
//!
//! The two jobs share one config, prepared at the event from the settings the algorithms chose
//! for the previous frame (this frame's statistics only exist at its end): a sliced stream
//! applies the algorithms' settings one frame later than a whole-frame one.

use std::time::{Duration, Instant};

use styx_native::SensorStream;
use styx_pisp::be::split_tiles;
use styx_pisp::device::{BackEndStream, FrontEndDevice, QueuedJob, profile};
use styx_pisp::uapi::BeTilesConfig;

use super::sensor_values;
use crate::error::{PipelineError, Result};
use crate::isp::IspSettings;
use crate::pisp_be::BeConfigBuilder;

/// Rows of a stripe's context the tiler adds below its output: stripes are cut this far above
/// the line so the first stripe's input ends by it.
const STRIPE_MARGIN: u16 = 32;

/// The tile stripe height for slicing at input row `line`.
pub(super) fn stripe_height(line: u16) -> u16 {
    line.saturating_sub(STRIPE_MARGIN)
}

/// A frame whose first job runs.
struct Pending {
    sequence: u32,
    input: u32,
    rest: Box<BeTilesConfig>,
    first: Option<QueuedJob>,
    /// Rows of each output final once `first` is done.
    rows_after_first: [u32; 2],
    at: Instant,
}

/// What slicing has done (for logs and metrics).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SliceCounts {
    /// Frames processed in two jobs.
    pub sliced: u64,
    /// Frames processed whole: no event in time, or the split did not fit.
    pub whole: u64,
    /// Line events with no sensor controls to prepare the config with.
    pub no_controls: u64,
    /// Line events whose config did not split at the line.
    pub no_split: u64,
    /// Line events seen.
    pub events: u64,
    /// Line events skipped: below the slice line, a frame still pending, no settings yet, or
    /// no raw buffer in flight.
    pub skipped: [u64; 4],
}

/// Slicing state of a running pipeline.
pub(super) struct Slicing {
    line: u32,
    /// The settings the algorithms chose for the last frame: the next frame's first job uses
    /// them (kept with `clone_from`, so no allocation per frame once warm).
    last: Option<IspSettings>,
    pending: Option<Pending>,
    pub(super) counts: SliceCounts,
}

impl Slicing {
    pub(super) fn new(line: u16) -> Self {
        Self {
            line: u32::from(line),
            last: None,
            pending: None,
            counts: SliceCounts::default(),
        }
    }

    /// A line event for frame `sequence`: starts its first job when the frame is the raw
    /// buffer in flight and the config splits at the line. Anything else leaves the frame to
    /// run whole.
    pub(super) fn on_line_event(
        &mut self,
        (sequence, line): (u32, u32),
        fe: &FrontEndDevice,
        be: &mut BackEndStream,
        builder: &mut BeConfigBuilder,
        sensor: &SensorStream,
    ) -> Result<()> {
        self.counts.events += 1;
        let skip = [
            line < self.line,
            self.pending.is_some(),
            self.last.is_none(),
            fe.image_in_flight().is_none(),
        ];
        if let Some(k) = skip.iter().position(|s| *s) {
            self.counts.skipped[k] += 1;
            return Ok(());
        }
        let (Some(settings), Some(input)) = (self.last.as_ref(), fe.image_in_flight()) else {
            return Ok(());
        };
        // The frame's own controls are recorded at its end; mid-frame the previous frame's
        // stand in (they only scale the temporal-denoise average, and match once AE settles).
        let seq = u64::from(sequence);
        let Some(controls) = sensor
            .applied(seq)
            .or_else(|| sensor.applied(seq.checked_sub(1)?))
        else {
            self.counts.no_controls += 1;
            return Ok(());
        };
        let values = sensor_values(seq, &controls);
        let exposure = values.exposure.as_secs_f64() * values.analogue_gain * settings.flicker;
        builder.update_frame(settings, exposure)?;
        let Some(split) = split_tiles(builder.config(), line) else {
            self.counts.no_split += 1;
            return Ok(());
        };
        let first = be.process_queued_part(input, &split.first, None, false)?;
        self.pending = Some(Pending {
            sequence,
            input,
            rest: Box::new(split.rest),
            first: Some(first),
            rows_after_first: split.rows_after_first,
            at: Instant::now(),
        });
        Ok(())
    }

    /// The frame in raw buffer `input` reached the back end with `settings` (the algorithms'
    /// for it): with its first job running, waits for that job and queues the rest into the
    /// same outputs; `None` when the frame was not split (the caller runs it whole). Either way
    /// `settings` become the next frame's.
    pub(super) fn submit_rest(
        &mut self,
        input: u32,
        settings: &IspSettings,
        be: &mut BackEndStream,
        timeout: Duration,
    ) -> Result<Option<QueuedJob>> {
        match &mut self.last {
            Some(last) => last.clone_from(settings),
            None => self.last = Some(settings.clone()),
        }
        if self.pending.as_ref().is_some_and(|p| p.input != input) {
            // Started on a raw buffer that is not this frame's: drop that job.
            self.abandon(be, timeout);
        }
        let Some(mut p) = self.pending.take() else {
            self.counts.whole += 1;
            return Ok(None);
        };
        let first = match p.first.take() {
            Some(j) => be.wait_job(j, timeout)?,
            None => return Ok(None),
        };
        self.counts.sliced += 1;
        let _ = (p.sequence, p.rows_after_first, p.at);
        Ok(Some(be.process_queued_part(
            input,
            &p.rest,
            Some(first.outputs),
            true,
        )?))
    }

    /// A frame that failed or was skipped: its first job, if any, is waited for and dropped.
    pub(super) fn abandon(&mut self, be: &mut BackEndStream, timeout: Duration) {
        if let Some(mut p) = self.pending.take()
            && let Some(j) = p.first.take()
            && let Ok(done) = be.wait_job(j, timeout)
        {
            be.release(&done);
        }
    }
}

impl Drop for Slicing {
    fn drop(&mut self) {
        if std::env::var("STYX_PISP_SLICE_LOG").as_deref() == Ok("1") {
            eprintln!("pisp slicing at line {}: {:?}", self.line, self.counts);
        }
    }
}

/// While slicing, the back end and its config builder, for the first job a line event starts.
pub(super) type SliceBe<'a> = (
    &'a mut Slicing,
    &'a mut BackEndStream,
    &'a mut BeConfigBuilder,
);

/// Serves the sensor side until the front end's statistics are ready, and while slicing starts
/// a frame's first job at its line event. Frame starts are only waited for while sensor writes
/// wait for one (a request whose values land a frame apart, e.g. frame length after exposure);
/// otherwise the statistics are the only wake of the frame (`next_held_raw` waits for them) and
/// the frame start is read when they arrive.
pub(super) fn wait_for_statistics(
    fe_dev: &FrontEndDevice,
    sensor: &SensorStream,
    timeout: Duration,
    mut slice: Option<SliceBe<'_>>,
) -> Result<()> {
    use styx_kernel::Wait;
    let deadline = Instant::now() + timeout;
    loop {
        sensor.service();
        let sync = sensor.frame_start_fd();
        let lines = slice.as_ref().and(fe_dev.line_event_fd());
        // Nothing to service while waiting: the dequeue below waits for the statistics.
        if sync.is_none() && lines.is_none() {
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        // The statistics, the sensor's frame starts and (while slicing) the line events: up
        // to three descriptors, in a list on the stack, as this waits on every frame.
        let mut fds = [(fe_dev.stats_fd(), Wait::READABLE); 3];
        let mut n = 1;
        let mut at_lines = None;
        if let Some(fd) = sync {
            fds[n] = (fd, Wait::PRIORITY);
            n += 1;
        }
        if let Some(fd) = lines {
            fds[n] = (fd, Wait::PRIORITY);
            at_lines = Some(n);
            n += 1;
        }
        let mut ready = [styx_kernel::Ready::default(); 3];
        profile::time("loop", "wait", || {
            styx_kernel::poll_into(&fds[..n], Some(left), &mut ready[..n])
        })
        .map_err(|e| PipelineError::Device(format!("waiting for the front end: {e}")))?;
        if let Some(k) = at_lines
            && ready[k].priority
            && let Some((s, be, builder)) = slice.as_mut()
        {
            while let Some(ev) = fe_dev.take_line_event() {
                // A frame whose first job cannot start runs whole at its end.
                let _ = s.on_line_event(ev, fe_dev, be, builder, sensor);
            }
        }
        // Statistics ready, or a node failing (the dequeue below reports it).
        if ready[0].readable || ready[..n].iter().any(|r| r.error || r.hangup) {
            return Ok(());
        }
    }
}
