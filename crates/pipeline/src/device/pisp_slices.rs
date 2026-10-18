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
use styx_pisp::be::split_tiles_into;
use styx_pisp::device::{BackEndStream, BeJob, FrontEndDevice, LineEvent, QueuedJob, profile};
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
    first: Option<QueuedJob>,
    /// The first job, once done (taken while waiting for the frame's statistics).
    done: Option<BeJob>,
    /// Rows of each output final once `first` is done.
    rows_after_first: [u32; 2],
    /// The frame's start (its buffer's timestamp to be).
    frame_start: Duration,
}

/// What [`super::PispPipeline::next_event`] delivers.
#[derive(Debug)]
// Unboxed: boxing the frame would allocate on every frame.
#[allow(clippy::large_enum_variant)]
pub enum PispEvent {
    /// A sliced frame's first rows, final.
    Slice(PispSlice),
    /// A frame, complete.
    Frame(super::PispFrame),
}

/// A frame's first slice is done: its outputs' buffers hold `rows` rows each (top to bottom),
/// final; the rest arrive with the frame ([`super::PispEvent::Frame`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PispSlice {
    /// The frame's sequence (as its [`super::PispFrame::sequence`]).
    pub sequence: u64,
    /// The frame's timestamp, as its [`super::PispFrame::timestamp`] will be (the start of
    /// frame, `CLOCK_MONOTONIC`).
    pub timestamp: Duration,
    /// Output buffer per output (the same the frame's job reports).
    pub outputs: [Option<u32>; 2],
    /// Rows final in each output.
    pub rows: [u32; 2],
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
    /// The split of the frame in flight, written in place at its line event (allocated once
    /// here, not per frame). `first` is read while its job is queued (the back end copies the
    /// config then); `rest` waits for the frame's end, and is only written while no frame is
    /// pending.
    first: Box<BeTilesConfig>,
    rest: Box<BeTilesConfig>,
    pub(super) counts: SliceCounts,
}

impl Slicing {
    pub(super) fn new(line: u16) -> Self {
        Self {
            line: u32::from(line),
            last: None,
            pending: None,
            first: Box::new(bytemuck::Zeroable::zeroed()),
            rest: Box::new(bytemuck::Zeroable::zeroed()),
            counts: SliceCounts::default(),
        }
    }

    /// A line event for frame `sequence`: starts its first job when the frame is the raw
    /// buffer in flight and the config splits at the line. Anything else leaves the frame to
    /// run whole.
    pub(super) fn on_line_event(
        &mut self,
        ev: LineEvent,
        fe: &FrontEndDevice,
        be: &mut BackEndStream,
        builder: &mut BeConfigBuilder,
        sensor: &SensorStream,
    ) -> Result<()> {
        let (sequence, line) = (ev.sequence, ev.line);
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
        let Some(rows_after_first) =
            split_tiles_into(builder.config(), line, &mut self.first, &mut self.rest)
        else {
            self.counts.no_split += 1;
            return Ok(());
        };
        let first = be.process_queued_part(input, &self.first, None, false)?;
        self.pending = Some(Pending {
            sequence,
            input,
            first: Some(first),
            done: None,
            rows_after_first,
            frame_start: ev.frame_start,
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
        let first = match (p.done.take(), p.first.take()) {
            (Some(done), _) => done,
            (None, Some(j)) => be.wait_job(j, timeout)?,
            (None, None) => return Ok(None),
        };
        self.counts.sliced += 1;
        Ok(Some(be.process_queued_part(
            input,
            &self.rest,
            Some(first.outputs),
            true,
        )?))
    }

    /// A frame that failed or was skipped: its first job, if any, is waited for and dropped.
    pub(super) fn abandon(&mut self, be: &mut BackEndStream, timeout: Duration) {
        let Some(mut p) = self.pending.take() else {
            return;
        };
        if let Some(done) = p.done.take() {
            be.release(&done);
        }
        if let Some(j) = p.first.take()
            && let Ok(done) = be.wait_job(j, timeout)
        {
            be.release(&done);
        }
    }

    /// Whether a first job is running (its output node becomes readable when done).
    pub(super) fn first_running(&self) -> bool {
        self.pending.as_ref().is_some_and(|p| p.first.is_some())
    }

    /// The running first job is done (its output node is readable): collects it and reports the
    /// frame's first slice.
    pub(super) fn first_done(
        &mut self,
        be: &mut BackEndStream,
        timeout: Duration,
    ) -> Result<Option<PispSlice>> {
        let Some(p) = self.pending.as_mut() else {
            return Ok(None);
        };
        let Some(j) = p.first.take() else {
            return Ok(None);
        };
        let done = be.wait_job(j, timeout)?;
        let slice = PispSlice {
            sequence: u64::from(p.sequence),
            timestamp: p.frame_start,
            outputs: done.outputs,
            rows: p.rows_after_first,
        };
        p.done = Some(done);
        Ok(Some(slice))
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

/// What [`wait_for_statistics`] woke for.
pub(super) enum Waited {
    /// The frame's statistics are ready (or a node failed: the dequeue reports it).
    Statistics,
    /// A sliced frame's first job is done.
    Slice(PispSlice),
}

/// Serves the sensor side until the front end's statistics are ready, and while slicing starts
/// a frame's first job at its line event. Frame starts are only waited for while sensor writes
/// wait for one (a request whose values land a frame apart, e.g. frame length after exposure);
/// otherwise the statistics are the only wake of the frame (`next_held_raw` waits for them) and
/// the frame start is read when they arrive.
/// With `slices`, returns early when a sliced frame's first job is done
/// ([`Waited::Slice`]).
pub(super) fn wait_for_statistics(
    fe_dev: &FrontEndDevice,
    sensor: &SensorStream,
    timeout: Duration,
    mut slice: Option<SliceBe<'_>>,
    slices: bool,
) -> Result<Waited> {
    use styx_kernel::Wait;
    let deadline = Instant::now() + timeout;
    loop {
        sensor.service();
        let left = deadline.saturating_duration_since(Instant::now());
        let (stats, lines, first, failed) = {
            let sync = sensor.frame_start_fd();
            let lines = slice.as_ref().and(fe_dev.line_event_fd());
            let first = slice
                .as_ref()
                .filter(|(s, _, _)| slices && s.first_running())
                .and_then(|(_, be, _)| be.output_fd(0));
            // Nothing to service while waiting: the dequeue waits for the statistics.
            if sync.is_none() && lines.is_none() && first.is_none() {
                return Ok(Waited::Statistics);
            }
            if left.is_zero() {
                return Ok(Waited::Statistics);
            }
            // The statistics, the sensor's frame starts, the line events and a sliced frame's
            // first job: up to four descriptors, in a list on the stack (this runs every frame).
            let mut fds = [(fe_dev.stats_fd(), Wait::READABLE); 4];
            let mut n = 1;
            let mut at = [None; 2];
            if let Some(fd) = sync {
                fds[n] = (fd, Wait::PRIORITY);
                n += 1;
            }
            for (k, fd) in [
                (0, lines.map(|f| (f, Wait::PRIORITY))),
                (1, first.map(|f| (f, Wait::READABLE))),
            ] {
                if let Some(fd) = fd {
                    fds[n] = fd;
                    at[k] = Some(n);
                    n += 1;
                }
            }
            let mut ready = [styx_kernel::Ready::default(); 4];
            profile::time("loop", "wait", || {
                styx_kernel::poll_into(&fds[..n], Some(left), &mut ready[..n])
            })
            .map_err(|e| PipelineError::Device(format!("waiting for the front end: {e}")))?;
            (
                ready[0].readable,
                at[0].is_some_and(|k| ready[k].priority),
                at[1].is_some_and(|k| ready[k].readable),
                ready[..n].iter().any(|r| r.error || r.hangup),
            )
        };
        if let Some((s, be, builder)) = slice.as_mut() {
            if lines {
                while let Some(ev) = fe_dev.take_line_event() {
                    // A frame whose first job cannot start runs whole at its end.
                    let _ = s.on_line_event(ev, fe_dev, be, builder, sensor);
                }
            }
            if first && let Some(done) = s.first_done(be, timeout)? {
                return Ok(Waited::Slice(done));
            }
        }
        // With the statistics in while a sliced frame's first job still runs (it ends just
        // after the readout at the best lines), the slice goes out first: the next call finds
        // the statistics ready.
        let first_pending = slices && slice.as_ref().is_some_and(|(s, _, _)| s.first_running());
        if failed || (stats && !first_pending) {
            return Ok(Waited::Statistics);
        }
    }
}
