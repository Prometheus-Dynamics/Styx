//! The per-frame processing loop, written once over the ISP traits: a frame goes through the
//! ISP with the newest settings the algorithms made (retargeted to the exposure the frame got),
//! its statistics go through the algorithms (while the ISP works, when the statistics come with
//! the frame), and their requests go to the camera's frame-exact control schedule.
//!
//! ```text
//!                  ┌── settings (newest step, retargeted to this frame) ──┐
//! frame F ─────────┴─► FrameIsp::submit ─► (between) ─► InlineIsp stats ──┼─► algorithms ─► requests
//!                                                       FrameIsp::finish ─┘   (F's stats)    (SensorControls)
//!                                           or FrameIsp::statistics after it (software, GPU ISP)
//! ```
//!
//! * [`FrameIsp`]: an ISP that processes a frame from memory into outputs: the PiSP back end,
//!   the software ISP, the GPU ISP. Statistics it gathers while processing (software, GPU) come
//!   after the job.
//! * [`InlineIsp`]: an ISP in the capture path whose statistics come with the frame and whose
//!   settings go into a queue ahead of it: the PiSP front end.
//! * [`Algorithms`]: which frames run the algorithms (every frame until settled, then about
//!   [`SETTLED_RATE_HZ`](crate::soft::SETTLED_RATE_HZ)), the settings for each frame, the
//!   algorithms on a frame's statistics with their requests applied, and [`Algorithms::process`]
//!   (one frame through the loop).
//! * [`SensorControls`]: where requests go (`styx-runtime`'s [`Controls`], the Linux camera's
//!   controls, a virtual sensor, or [`NoControls`]).
//!
//! `no_std` + `alloc`, deterministic (the same inputs give the same outputs bit for bit).

use alloc::format;
use core::time::Duration;

use styx_algo::{LensRequest, LensState, PdafZone, SensorRequest, Statistics, ZoneGrid};
use styx_runtime::Controls;
use styx_sensor::{ControlRequest, RegisterBus, SensorPins};

use crate::controller::{Controller, SensorValues, Start, Step};
use crate::error::{PipelineError, Result};
use crate::isp::IspSettings;
use crate::time::now;

/// Where the loop's requests go: a camera's frame-exact control schedule.
pub trait SensorControls {
    /// Applies exposure, analogue gain and frame duration together from `r.frame`; returns the
    /// frame the last of them lands on.
    fn request(&self, r: &SensorRequest) -> Result<u64>;
    /// Moves the focus lens for `r.frame`.
    fn lens(&self, r: &LensRequest) -> Result<()>;
}

/// No camera (a replay on a host): requests land where they ask to.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoControls;

impl SensorControls for NoControls {
    fn request(&self, r: &SensorRequest) -> Result<u64> {
        Ok(r.frame)
    }
    fn lens(&self, _r: &LensRequest) -> Result<()> {
        Ok(())
    }
}

/// The control request of an algorithms' sensor request.
pub fn control_request(r: &SensorRequest) -> ControlRequest {
    ControlRequest {
        exposure: Some(r.exposure),
        gain: Some(r.analogue_gain),
        frame_duration: Some(r.frame_duration),
    }
}

impl<B: RegisterBus, P: SensorPins> SensorControls for Controls<B, P> {
    /// What is due in the current frame is written at once while enough of it is left; before
    /// streaming the values for frame 0 are written at once. No allocation.
    fn request(&self, r: &SensorRequest) -> Result<u64> {
        let landings = self
            .request_at_now_landings(r.frame, &control_request(r))
            .map_err(|e| PipelineError::Device(format!("{e}")))?;
        Ok(landings.iter().map(|l| l.frame).max().unwrap_or(r.frame))
    }

    fn lens(&self, r: &LensRequest) -> Result<()> {
        self.request_lens_at(r.frame, r.position)
            .map_err(|e| PipelineError::Device(format!("{e}")))
    }
}

/// An ISP that processes a frame from memory into output buffers: the PiSP back end, the
/// software ISP, the GPU ISP, a vendor memory-to-memory ISP.
pub trait FrameIsp {
    /// A frame to process (a receiver buffer index, bytes, a dma-buf, with its outputs).
    type Input<'a>;
    /// A job in progress.
    type Job;
    /// A finished job's outputs.
    type Output;

    /// Starts processing `input` (the frame `values` describes) with `settings`; with
    /// `statistics` the ISP also gathers the frame's statistics, if it makes any.
    fn submit(
        &mut self,
        input: Self::Input<'_>,
        settings: &IspSettings,
        values: &SensorValues,
        statistics: bool,
    ) -> Result<Self::Job>;

    /// Waits for `job`.
    fn finish(&mut self, job: Self::Job) -> Result<Self::Output>;

    /// Converts the statistics gathered with the last finished job into `out`; `false` if it
    /// made none.
    fn statistics(&mut self, out: &mut Statistics) -> bool {
        let _ = out;
        false
    }

    /// Gives a finished job's outputs back (its frame failed).
    fn discard(&mut self, output: &Self::Output) {
        let _ = output;
    }
}

/// An ISP in the capture path: statistics come with the frame, settings go into a queue ahead
/// of the frame (the PiSP front end, rkisp1, an STM32 DCMIPP).
pub trait InlineIsp {
    /// Converts the statistics delivered with the current frame into `out`; `false` if there
    /// are none (not captured for this frame).
    fn statistics(&mut self, out: &mut Statistics) -> bool;
    /// Settings for the frames it takes next.
    fn set_settings(&mut self, settings: &IspSettings);
}

/// No inline ISP.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoInline;

impl InlineIsp for NoInline {
    fn statistics(&mut self, _out: &mut Statistics) -> bool {
        false
    }
    fn set_settings(&mut self, _settings: &IspSettings) {}
}

/// Records a timing (`what`, `op`, how long) for profiles.
pub type Profiler = fn(&'static str, &'static str, Duration);

/// Where one frame's time went in [`Algorithms::process`] (zero without `std`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProcessTimes {
    /// The settings for the frame (retargeted to its exposure).
    pub settings: Duration,
    /// Submitting the frame to the ISP (preparing its configuration; the whole job for an ISP
    /// that works on the calling thread).
    pub submit: Duration,
    /// Converting the statistics for the algorithms.
    pub stats: Duration,
    /// The algorithms and their requests.
    pub algorithms: Duration,
    /// Waiting for the ISP job.
    pub finish: Duration,
}

/// One frame through [`Algorithms::process`].
#[derive(Debug)]
pub struct Processed<O> {
    /// The ISP's outputs.
    pub output: O,
    /// The frame whose statistics the settings came from (`None`: the start-up values).
    pub settings_from: Option<u64>,
    /// The digital gain the frame was given (with the white balance's green gain).
    pub digital_gain: f64,
    /// The flicker brightness deflicker took out of this frame (1: none).
    pub flicker: f64,
    /// The algorithms ran on this frame's statistics.
    pub ran: bool,
    /// The frame the sensor request made from this frame's statistics lands on.
    pub request_lands: Option<u64>,
    /// Where the time went.
    pub times: ProcessTimes,
}

/// The 3A side of the loop: see the [module documentation](self).
#[derive(Debug)]
pub struct Algorithms {
    controller: Controller,
    /// The newest output; its `isp` processes the next frame.
    step: Step,
    /// `step` came from a frame's statistics (not the start-up values).
    stepped: bool,
    /// `step.isp` was taken ([`Self::take_step`]): rebuilt from the params for the next frame.
    isp_taken: bool,
    black_level: f64,
    /// The frame the algorithms last ran on.
    last_run: Option<u64>,
    /// While settled, the algorithms run every this many frames.
    settled_every: u64,
    stats: Statistics,
    profiler: Option<Profiler>,
}

impl Algorithms {
    /// The loop around `controller`, with neutral settings for a sensor at `black_level`
    /// until [`Self::start`].
    pub fn new(controller: Controller, black_level: f64) -> Self {
        Self {
            controller,
            step: Step {
                frame: 0,
                sensor: None,
                lens: None,
                isp: IspSettings::neutral(black_level),
                params: Default::default(),
            },
            stepped: false,
            isp_taken: false,
            black_level,
            last_run: None,
            settled_every: 1,
            stats: Statistics::default(),
            profiler: None,
        }
    }

    /// Records the algorithms' run time (`loop`, `algorithms`) with `profiler`.
    pub fn set_profiler(&mut self, profiler: Option<Profiler>) {
        self.profiler = profiler;
    }

    /// The controller.
    pub fn controller(&mut self) -> &mut Controller {
        &mut self.controller
    }

    /// The controller, shared.
    pub fn controller_ref(&self) -> &Controller {
        &self.controller
    }

    /// The newest output (from the statistics of frame `step().frame`, or the start-up values).
    pub fn step(&self) -> &Step {
        &self.step
    }

    /// Whether [`Self::step`] came from a frame's statistics.
    pub fn stepped(&self) -> bool {
        self.stepped
    }

    /// The statistics of the frame the algorithms last ran on.
    pub fn statistics(&self) -> &Statistics {
        &self.stats
    }

    /// The newest output, its settings moved out (no copy): the next frame's settings are
    /// built from its params again. For callers that hand each new step on (the software loop).
    pub fn take_step(&mut self) -> Step {
        let isp = core::mem::replace(&mut self.step.isp, IspSettings::neutral(self.black_level));
        self.isp_taken = self.stepped;
        Step {
            frame: self.step.frame,
            sensor: self.step.sensor,
            lens: self.step.lens,
            isp,
            params: self.step.params.clone(),
        }
    }

    /// Takes the statistics of the frame the algorithms last ran on (leaving them empty).
    pub fn take_statistics(&mut self) -> Statistics {
        core::mem::take(&mut self.stats)
    }

    /// While settled the algorithms run every `frames` frames (1: every frame).
    pub fn set_settled_every(&mut self, frames: u64) {
        self.settled_every = frames.max(1);
    }

    /// While settled the algorithms run at about `rate_hz` of frames at `fps` (`None`: every
    /// frame).
    pub fn set_settled_rate(&mut self, fps: f64, rate_hz: Option<f64>) {
        self.settled_every = rate_hz.filter(|r| *r > 0.0 && fps > 0.0).map_or(1, |r| {
            let n = fps / r;
            #[cfg(not(feature = "std"))]
            use crate::math::Float as _;
            n.round().max(1.0) as u64
        });
    }

    /// Resets the algorithms (from the controller's warm start) and hands their start-up
    /// request for frame 0 to `controls` (written before streaming); the start-up settings
    /// process the first frames.
    pub fn start(&mut self, controls: &impl SensorControls) -> Result<Start> {
        let start = self.controller.start()?;
        if let Some(r) = &start.sensor {
            controls.request(r)?;
        }
        if let Some(l) = &start.lens {
            controls.lens(l)?;
        }
        self.step = Step {
            frame: 0,
            sensor: start.sensor,
            lens: start.lens,
            isp: start.isp.clone(),
            params: Default::default(),
        };
        self.stepped = false;
        self.isp_taken = false;
        self.last_run = None;
        Ok(start)
    }

    /// Whether the algorithms run on frame `next` (`None`: not known yet, they run): on every
    /// frame until AE has locked and AWB has converged (and with AE on), then every
    /// [`Self::set_settled_every`] frames.
    pub fn due(&self, next: Option<u64>) -> bool {
        let p = &self.step.params;
        let settled = self.stepped
            && p.ae.locked
            && p.awb.converged
            && self.controller.controls().ae_enable
            && !p.needs_every_frame();
        !settled
            || match (next, self.last_run) {
                (Some(f), Some(r)) => f >= r + self.settled_every,
                _ => true,
            }
    }

    /// The settings for the frame `values` describes: the newest the algorithms made, with
    /// the digital gain (and deflicker's correction) for the exposure that frame got.
    pub fn settings(&mut self, values: &SensorValues) -> &IspSettings {
        if self.isp_taken {
            // The same settings retargeting would give: from the params, for this frame.
            self.step.isp = self
                .controller
                .isp_for(&self.step.params, self.step.frame, values);
            self.isp_taken = false;
        } else if self.stepped {
            self.controller
                .retarget(&mut self.step.isp, &self.step.params, values);
        }
        &self.step.isp
    }

    /// Runs the algorithms on `stats` (of the frame `values` describes; `lens`: where the
    /// lens was) and hands their requests to `controls`. Returns the frame the sensor request
    /// lands on, if one was made.
    pub fn run(
        &mut self,
        stats: &Statistics,
        values: &SensorValues,
        lens: Option<LensState>,
        controls: &impl SensorControls,
    ) -> Result<Option<u64>> {
        let t = now();
        let step = self.controller.process_with_lens(stats, values, lens)?;
        if let Some(p) = self.profiler {
            p("loop", "algorithms", now().since(t));
        }
        let lands = match &step.sensor {
            Some(r) => Some(controls.request(r)?),
            None => None,
        };
        if let Some(l) = &step.lens {
            controls.lens(l)?;
        }
        self.step = step;
        self.stepped = true;
        self.isp_taken = false;
        self.last_run = Some(values.frame);
        Ok(lands)
    }

    /// One frame through the loop: the settings for it, the ISP job, `between` (with the job
    /// and the settings' step: a raw copy, serving frame starts that came meanwhile), the
    /// algorithms on the inline ISP's statistics while the job runs (then the inline ISP takes
    /// the new settings), the job's end, and the algorithms on the frame ISP's statistics when
    /// there were no inline ones. `run` says whether the algorithms run on this frame
    /// ([`Self::due`]); `focus` is where the lens was and the frame's phase detection data.
    #[allow(clippy::too_many_arguments)]
    pub fn process<I: FrameIsp, F: InlineIsp, C: SensorControls>(
        &mut self,
        isp: &mut I,
        inline: &mut F,
        controls: &C,
        input: I::Input<'_>,
        values: &SensorValues,
        run: bool,
        focus: (Option<LensState>, Option<ZoneGrid<PdafZone>>),
        between: impl FnOnce(&I::Job, &Step),
    ) -> Result<Processed<I::Output>> {
        let mut times = ProcessTimes::default();
        let settings_from = self.stepped.then_some(self.step.frame);
        let t = now();
        let settings = self.settings(values);
        let (digital_gain, flicker) = (settings.digital_gain, settings.flicker);
        let t1 = now();
        times.settings = t1.since(t);
        let job = isp.submit(input, settings, values, run)?;
        let t2 = now();
        times.submit = t2.since(t1);
        between(&job, &self.step);
        let (lens, pdaf) = focus;
        let mut pdaf = Some(pdaf);
        // While the ISP works: the statistics that came with the frame through the algorithms.
        let mut ran = None;
        if run {
            let t = now();
            if inline.statistics(&mut self.stats) {
                self.stats.pdaf = pdaf.take().flatten();
                let t1 = now();
                times.stats = t1.since(t);
                let r = self.run_stats(values, lens, controls);
                times.algorithms = now().since(t1);
                if r.is_ok() {
                    inline.set_settings(&self.step.isp);
                }
                ran = Some(r);
            }
        }
        let t = now();
        let output = isp.finish(job)?;
        times.finish = now().since(t);
        if let Some(Err(e)) = ran {
            isp.discard(&output);
            return Err(e);
        }
        // Statistics the ISP made while processing (software, GPU): settings for the next.
        if run && ran.is_none() {
            let t = now();
            let mut stats = Statistics::default();
            if isp.statistics(&mut stats) {
                stats.pdaf = pdaf.take().flatten();
                self.stats = stats;
                let t1 = now();
                times.stats = t1.since(t);
                let r = self.run_stats(values, lens, controls);
                times.algorithms = now().since(t1);
                match r {
                    Ok(lands) => ran = Some(Ok(lands)),
                    Err(e) => {
                        isp.discard(&output);
                        return Err(e);
                    }
                }
            }
        }
        let (ran, request_lands) = match ran {
            Some(Ok(lands)) => (true, lands),
            _ => (false, None),
        };
        Ok(Processed {
            output,
            settings_from,
            digital_gain,
            flicker,
            ran,
            request_lands,
            times,
        })
    }

    fn run_stats(
        &mut self,
        values: &SensorValues,
        lens: Option<LensState>,
        controls: &impl SensorControls,
    ) -> Result<Option<u64>> {
        let stats = core::mem::take(&mut self.stats);
        let r = self.run(&stats, values, lens, controls);
        self.stats = stats;
        r
    }
}
