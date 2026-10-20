//! The PiSP path on a native camera.
//!
//! ```text
//! sensor ─► csi2 ─► pisp-fe ─► fe_stats  (every frame) ─► stats::from_pisp_raw ─► Controller
//!                      │                                                           │   │
//!                      └─► fe_image0 (16-bit raw, held) ═dma-buf═► pispbe ◄────────┘   └─► SensorRequest
//!                                                                   ├─► output0 (e.g. NV12 1280x800)
//!                                                                   └─► output1 (e.g. RGB 640x400)
//! ```
//!
//! The frame path is kept short: when frame F's statistics and raw frame arrive, F goes
//! through the back end at once with the newest settings the algorithms have made (those from
//! F − 1's statistics), its digital gain recomputed for the exposure F actually got, and the
//! back end config patched only where the settings changed ([`BeConfigBuilder`]). While the
//! back end works (0.85 ms for 1280x800), F's statistics go through the algorithms and their
//! sensor request is issued, as early as before; their run time (up to 0.7 ms on the frames
//! AWB and lens shading run) is hidden behind the back end job. The order of inputs and
//! outputs is fixed, so the loop stays deterministic. The front end's RGB-to-Y weights and
//! black levels follow on the next config it takes (configs are queued a couple of frames
//! ahead).

mod frame;
mod outputs;
mod raw_lease;
pub use raw_lease::{RAW_LEASE_BUFFERS, RawLayout};
mod passes;

pub use frame::{PassOutput, PispFrame, PispStartup, PispTimes};
use std::time::{Duration, Instant};

use styx_algo::{Statistics, Tuning, WarmStart};
use styx_kernel::subdev::MbusCode;
use styx_native::{
    CameraControls, Configured, NativeCamera, SensorStream, StreamSettings, select_mode,
};
use styx_pisp::device::{BackEndStream, FrontEndDevice, FrontEndSetup, profile};
use styx_pisp::fe::FrontEnd;
use styx_pisp::uapi::{BayerOrder, ImageFormatConfig, RawStatistics, fe_enable};
use styx_softisp::CfaPattern;

use super::pisp_isp::{BackEnd, FeStats};
use super::pisp_options::PispOptions;
use super::pisp_slices::{PispEvent, Waited};
use super::{Profiled, sensor_values};
use crate::controller::{Controller, Step};
use crate::error::{PipelineError, Result};
use crate::isp::{be_template, level16};
use crate::pisp_be::{BeConfigBuilder, BeUpdateCounts};
use crate::pisp_passes::PassConfigs;
use crate::process::Algorithms;
use crate::sensor::{ISSUE_LATENCY, SensorInfo};

fn bayer(c: CfaPattern) -> BayerOrder {
    match c {
        CfaPattern::Rggb => BayerOrder::Rggb,
        CfaPattern::Bggr => BayerOrder::Bggr,
        CfaPattern::Grbg => BayerOrder::Grbg,
        CfaPattern::Gbrg => BayerOrder::Gbrg,
    }
}

/// Debug hook (feature `pisp-debug-hooks`): the environment variable `name`, parsed. Without
/// the feature it is ignored, so the options are the only way to set these.
#[cfg(feature = "pisp-debug-hooks")]
fn debug_var<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok()?.parse().ok()
}

/// See the feature-enabled version: the environment is not read.
#[cfg(not(feature = "pisp-debug-hooks"))]
fn debug_var<T>(_name: &str) -> Option<T> {
    None
}

/// The front and back end, opened for a sensor mode.
struct Isp {
    fe: FrontEnd,
    fe_dev: FrontEndDevice,
    be_dev: BackEndStream,
    be: BeConfigBuilder,
    /// Why temporal denoise could not be set up, if it was wanted.
    tdn_error: Option<String>,
    /// The line frames are split at ([`PispOptions::slice_line`]), when the front end reports
    /// it; else why not.
    slice: std::result::Result<Option<u16>, String>,
}

impl Isp {
    /// Front end (links, formats, buffers), back end (formats, buffers, the front end's raw
    /// buffers imported) and the back end template, for `info`'s size and colour order on
    /// media bus code `code`. Returns how long each end took.
    fn open(
        info: &SensorInfo,
        code: u32,
        options: &PispOptions,
        want_tdn: bool,
    ) -> Result<(Self, Duration, Duration)> {
        let t = Instant::now();
        let order = bayer(info.cfa);
        // The receiver's and front end's device formats carry the order of the sensor pad's
        // code, which a kernel driver keeps when its flips change the data's order; the front
        // and back end configuration, the data's.
        let device_order = styx_sensor::MbusCode(code)
            .color_filter()
            .and_then(|c| crate::sensor::cfa(c).ok())
            .map_or(order, bayer);
        let want_slice = options
            .slice_line
            .filter(|l| *l > 0 && u32::from(*l) < info.height);
        let mut fe_dev = FrontEndDevice::open(&FrontEndSetup {
            width: info.width,
            height: info.height,
            sensor_code: MbusCode(code),
            bayer: device_order,
            image_output: true,
            buffers: options.fe_buffers,
            keep_embedded: true,
        })?;
        let fe_open = t.elapsed();
        let t = Instant::now();
        let input = fe_dev.image_format();
        let mut fe = FrontEnd::new(info.width as u16, info.height as u16, order);
        fe.default_stats(level16(info.black_level), 1.0, 1.0);
        fe.set_output_format(0, input);
        // Slicing: the front end's line event (Styx's rp1-cfe module) at the slice line.
        let slice = match want_slice {
            Some(line) => match fe_dev.enable_line_events() {
                Ok(()) => {
                    fe.set_ilines(0, line);
                    Ok(Some(line))
                }
                Err(e) => Err(format!("no line events from the front end: {e}")),
            },
            None => Ok(None),
        };
        // Debug: the line the raw output's line interrupt fires at (Styx's rp1-cfe module).
        if let Some(n) = debug_var::<u16>("STYX_PISP_FE_ILINES") {
            fe.set_ilines(0, n);
        }
        fe.enable(fe_enable::OUTPUT0, true);
        let input_len = input.stride as u32 * u32::from(input.height);
        let initial = options.be_buffers_initial.unwrap_or(options.be_buffers);
        let mut be_dev = BackEndStream::open_with_initial(
            options.be_group,
            input,
            order,
            fe_dev.image_dmabufs()?,
            input_len,
            options.outputs,
            [
                options.be_buffers + options.pass_buffers,
                options.be_buffers,
            ],
            [initial + options.pass_buffers, initial],
            options.output_memory,
            options.config_memory,
        )?;
        let mut be = be_template(
            input,
            order,
            info.black_level,
            [be_dev.output_format(0), be_dev.output_format(1)],
        )?;
        // Debug: horizontal stripes of at most N rows (measuring what slicing costs).
        if let Some(n) = debug_var::<u16>("STYX_PISP_BE_STRIPE") {
            be.set_max_stripe_height(n);
        }
        if let Ok(Some(line)) = slice {
            be.set_max_stripe_height(super::pisp_slices::stripe_height(line));
        }
        for (i, crop) in options.crop.iter().enumerate() {
            be.set_crop(i, crop.unwrap_or_default());
        }
        // The template must prepare (sizes, strides, tiles) before streaming starts.
        let mut be = BeConfigBuilder::new(be)?;
        let mut tdn_error = None;
        if want_tdn && options.temporal_denoise {
            match be_dev.enable_tdn() {
                Ok(()) => be.enable_tdn(be_dev.tdn_format(options.tdn_compressed)),
                Err(e) => tdn_error = Some(e.to_string()),
            }
        }
        Ok((
            Self {
                fe,
                fe_dev,
                be_dev,
                be,
                tdn_error,
                slice,
            },
            fe_open,
            t.elapsed(),
        ))
    }
}

/// A native camera through the PiSP. See the [module documentation](self).
pub struct PispPipeline {
    camera: NativeCamera,
    controls: CameraControls,
    configured: Configured,
    info: SensorInfo,
    /// The 3A side of the loop (`crate::process`): settings, scheduling, the algorithms.
    algo: Algorithms,
    fe: FrontEnd,
    fe_dev: Option<FrontEndDevice>,
    /// Frames split into two back end jobs ([`PispOptions::slice_line`]), when on.
    slicing: Option<super::pisp_slices::Slicing>,
    /// Why slicing is off although asked for.
    slice_error: Option<String>,
    /// The window of the sensor's field the mode reads, if it reads one.
    sensor_window: Option<styx_pisp::device::SensorWindow>,
    be_dev: Option<BackEndStream>,
    be: BeConfigBuilder,
    sensor: Option<SensorStream>,
    options: PispOptions,
    startup: PispStartup,
    warm_override: Option<Option<WarmStart>>,
    /// The last frame's statistics, as copied from the front end.
    raw_stats: Box<RawStatistics>,
    /// The tuning has temporal denoise (the back end then sets up its buffers).
    want_tdn: bool,
    /// Why the back end's temporal denoise could not be set up.
    tdn_error: Option<String>,
    /// The last frame dequeued.
    last_seq: Option<u64>,
    /// Which frames' raw data to copy out (stills).
    raw_copy: Option<super::RawCopy>,
    /// Extra back end passes over every frame (regions of interest).
    passes: PassConfigs,
    /// The front end's raw buffers as dma-bufs, with [`PispOptions::raw_lease`].
    raw_fds: Vec<std::os::fd::OwnedFd>,
}

/// Time from the end of a frame's readout until its statistics have been through the
/// algorithms and the request is ready (front end statistics, dequeue, algorithms), with slack.
const PISP_PROCESSING: Duration = Duration::from_millis(2);

impl std::fmt::Debug for PispPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PispPipeline")
            .field("configured", &self.configured)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl PispPipeline {
    /// Sets the camera, the front end and the back end up for `settings` (sensor size and
    /// rate) with the algorithms of `tuning`. The sensor's bring-up (power, I²C init and mode)
    /// runs on another thread while the front and back end open.
    pub fn open(
        mut camera: NativeCamera,
        settings: &StreamSettings,
        tuning: &Tuning,
        options: PispOptions,
    ) -> Result<Self> {
        let t_open = Instant::now();
        let want_tdn = tuning.denoise.as_ref().is_some_and(|d| d.tdn.is_some());
        let mut startup = PispStartup::default();
        let mode = select_mode(&camera.info().modes, settings, &camera.info().raw_formats)?.clone();
        let desc = std::sync::Arc::clone(&camera.info().description);
        let sensor_info = SensorInfo::from_description(&desc, &mode.mode, &mode.format)?
            .with_flips(&desc, settings.hflip, settings.vflip)?;
        // The code the front end's input carries (the receiver checks it against the sensor's).
        let code = camera.bus_code(mode.code, settings.hflip, settings.vflip);
        let (configured, isp) = std::thread::scope(|scope| {
            let sensor = scope.spawn(|| {
                let t = Instant::now();
                let c = camera.configure_external(settings);
                (c, t.elapsed())
            });
            let isp = Isp::open(&sensor_info, code, &options, want_tdn);
            let (configured, took) = sensor.join().unwrap_or((
                Err(styx_native::NativeError::State("sensor set-up panicked")),
                Duration::ZERO,
            ));
            startup.configure = took;
            (configured, isp)
        });
        let configured = configured?;
        let (isp, fe_open, be_open) = isp?;
        startup.bring_up = camera.bring_up_times();
        startup.fe_open = fe_open;
        startup.be_open = be_open;
        let t = Instant::now();
        let fps = configured.interval.fps();
        // The description after configuring: a kernel driver's sensor reports the exact
        // ranges of the mode only once it is set.
        let desc = std::sync::Arc::clone(&camera.info().description);
        let mut info =
            SensorInfo::from_description(&desc, &configured.mode.mode, &configured.mode.format)?
                .with_flips(&desc, configured.hflip, configured.vflip)?
                .with_fps(fps, fps)?;
        info.camera.temporal_denoise = isp.be_dev.tdn_enabled();
        info.camera.lens = super::lens_config(&camera);
        let mut controller = Controller::new(tuning, info.camera.clone())?;
        controller.set_spatial_denoise(options.spatial_denoise);
        let controls = camera.controls();
        startup.isp_and_algorithms = t.elapsed();
        startup.open = t_open.elapsed();
        let passes = PassConfigs::new(
            [isp.be_dev.output_format(0), isp.be_dev.output_format(1)],
            options.pass_tdn,
        );
        let mut p = Self {
            camera,
            controls,
            configured,
            algo: {
                let mut a = Algorithms::new(controller, info.black_level);
                a.set_profiler(Some(profile_record));
                a
            },
            info,
            fe: isp.fe,
            sensor_window: isp.fe_dev.sensor_window(),
            fe_dev: Some(isp.fe_dev),
            be_dev: Some(isp.be_dev),
            be: isp.be,
            sensor: None,
            options,
            startup,
            warm_override: None,
            raw_stats: bytemuck::allocation::zeroed_box(),
            want_tdn,
            tdn_error: isp.tdn_error,
            slicing: isp
                .slice
                .clone()
                .ok()
                .flatten()
                .map(super::pisp_slices::Slicing::new),
            slice_error: isp.slice.err(),
            last_seq: None,
            raw_copy: None,
            raw_fds: Vec::new(),
            passes,
        };
        p.export_raw()?;
        Ok(p)
    }

    /// Whether the back end runs temporal denoise, and why not if the tuning asked for it.
    pub fn temporal_denoise(&self) -> (bool, Option<&str>) {
        (self.info.camera.temporal_denoise, self.tdn_error.as_deref())
    }

    /// Why frames are not split although [`PispOptions::slice_line`] asked for it (no line
    /// events: the receiver is not Styx's rp1-cfe module). They are delivered whole.
    pub fn slice_error(&self) -> Option<&str> {
        self.slice_error.as_deref()
    }

    /// Where opening and the last start spent their time.
    pub fn startup(&self) -> &PispStartup {
        &self.startup
    }

    /// The configuration in effect.
    pub fn configured(&self) -> &Configured {
        &self.configured
    }

    /// The window of the sensor's field the mode reads (a mode of fewer lines at full pitch),
    /// `None` when it reads the whole field. Frames report their `FrameMeta::crop` in the full
    /// field's coordinates from it.
    pub fn sensor_window(&self) -> Option<styx_pisp::device::SensorWindow> {
        self.sensor_window
    }

    /// The sensor values of frame `seq` before it completes (a sliced frame's first slice):
    /// what the control schedule applied, else predicts, else the previous frame's.
    pub fn frame_values(&self, seq: u64) -> Option<crate::controller::SensorValues> {
        let s = self.sensor.as_ref()?;
        let c = s.applied(seq).or_else(|| s.applied(seq.checked_sub(1)?))?;
        Some(sensor_values(seq, &c))
    }

    /// The sensor mode.
    pub fn info(&self) -> &SensorInfo {
        &self.info
    }

    /// The controller (controls, recording).
    pub fn controller(&mut self) -> &mut Controller {
        self.algo.controller()
    }

    /// New controls from the next frame on (see [`crate::Algorithms::set_controls`]).
    pub fn set_controls(&mut self, controls: styx_algo::Controls) {
        self.algo.set_controls(controls);
    }

    /// The camera's controls.
    pub fn controls(&self) -> &CameraControls {
        &self.controls
    }

    /// The statistics of the frame [`Self::next`] returned last, as the algorithms see them.
    pub fn statistics(&self) -> &Statistics {
        self.algo.statistics()
    }

    /// The algorithms' newest output, from the statistics of the frame [`Self::next`] returned
    /// last (`step.frame`); its `isp` settings process the next frame.
    pub fn step(&self) -> &Step {
        self.algo.step()
    }

    /// How often the back end config was rebuilt, patched or reused.
    pub fn be_updates(&self) -> BeUpdateCounts {
        self.be.counts()
    }

    /// The back end config (and tiles) of the last job [`Self::next`] returned.
    pub fn back_end_config(&self) -> &styx_pisp::uapi::BeTilesConfig {
        self.be.config()
    }

    /// Back end output `i`'s format.
    pub fn output_format(&self, i: usize) -> Option<ImageFormatConfig> {
        self.be_dev.as_ref()?.output_format(i)
    }

    /// Where the back end's config buffer comes from (`mmap` or a dma-heap).
    pub fn back_end_config_source(&self) -> Option<String> {
        self.be_dev.as_ref().map(BackEndStream::config_source)
    }

    /// Whether frame starts come from `FRAME_SYNC` events and embedded data is read back.
    pub fn sensor_feedback(&self) -> (bool, bool) {
        self.sensor.as_ref().map_or((false, false), |s| {
            (s.uses_frame_sync(), s.has_embedded_data())
        })
    }

    /// Starts the next [`Self::start`] (only) from these settled values (`None`: from the
    /// tuning's start-up values) instead of what the camera's last session settled on
    /// ([`crate::warm::recall`], the default).
    pub fn set_warm_start(&mut self, warm: Option<WarmStart>) {
        self.warm_override = Some(warm);
    }

    /// Resets the algorithms (from the camera's last settled state, see [`crate::warm`]),
    /// requests their start-up values for frame 0 (written before streaming) and starts
    /// streaming. Sensor requests are written as soon as they are made: in the frame the
    /// statistics came from while enough of it is left (frame starts from `FRAME_SYNC`), so a
    /// change lands a control delay after the frame that asked for it.
    pub fn start(&mut self) -> Result<()> {
        let t_start = Instant::now();
        if self.sensor.is_some() {
            return Err(PipelineError::Device("already started".into()));
        }
        // After a stop the camera is still configured and powered; only the ISP reopens.
        if self.fe_dev.is_none() || self.be_dev.is_none() {
            self.fe_dev = None;
            self.be_dev = None;
            let (isp, fe_open, be_open) = Isp::open(
                &self.info,
                self.configured.mode.code,
                &self.options,
                self.want_tdn,
            )?;
            self.tdn_error = isp.tdn_error;
            self.slicing = isp
                .slice
                .clone()
                .ok()
                .flatten()
                .map(super::pisp_slices::Slicing::new);
            self.slice_error = isp.slice.clone().err();
            if isp.be_dev.tdn_enabled() != self.info.camera.temporal_denoise {
                self.info.camera.temporal_denoise = isp.be_dev.tdn_enabled();
                self.algo
                    .controller()
                    .set_config(self.info.camera.clone())?;
            }
            (self.fe, self.be) = (isp.fe, isp.be);
            // A new builder: the passes prepare again against it.
            self.passes.invalidate();
            self.fe_dev = Some(isp.fe_dev);
            self.export_raw()?;
            self.be_dev = Some(isp.be_dev);
            (self.startup.fe_open, self.startup.be_open) = (fe_open, be_open);
        }
        let t_ext = Instant::now();
        let fe_dev = self
            .fe_dev
            .as_mut()
            .ok_or_else(|| PipelineError::Device("stopped".into()))?;
        let sync = fe_dev
            .image_node_path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| PipelineError::Device("no fe_image0 node".into()))?;
        // This thread serves frame starts and embedded data itself while it waits for frames
        // (the event thread only answers the bridge): one wake per frame for the front end.
        let sensor = self.camera.start_external_driven(&sync)?;
        self.startup.start_external = t_ext.elapsed();
        let t = Instant::now();
        let latency = if sensor.uses_frame_sync() {
            self.info
                .issue_latency(PISP_PROCESSING, styx_native::control::DEFAULT_WRITE_MARGIN)
        } else {
            ISSUE_LATENCY
        };
        self.sensor = Some(sensor);
        self.algo.controller().set_issue_latency(latency);
        self.controls.set_loop_latency(Some(latency));
        let warm = match self.warm_override.take() {
            Some(w) => w,
            None => crate::warm::recall(&self.camera.info().key),
        };
        self.algo.controller().set_warm_start(warm);
        let values = self
            .algo
            .start(&self.controls)
            .map(|start| start.isp.apply_fe(&mut self.fe));
        let fps = self.configured.interval.fps();
        self.algo
            .set_settled_rate(fps, self.options.settled_rate_hz);
        self.last_seq = None;
        if let Err(e) = values {
            self.camera.quiesce_external();
            let _ = self.camera.stop();
            self.sensor = None;
            return Err(e);
        }
        self.startup.start_values = t.elapsed();
        let t = Instant::now();
        let Some(fe_dev) = self.fe_dev.as_mut() else {
            return Err(PipelineError::Device("stopped".into()));
        };
        // The event thread is quiesced while the front end's STREAMON waits for the sensor.
        let started = fe_dev
            .start(&mut self.fe, self.options.configs_ahead)
            .map_err(PipelineError::from)
            .and_then(|()| self.camera.resume_external().map_err(PipelineError::from));
        if let Err(e) = started {
            self.camera.quiesce_external();
            let _ = self.fe_dev.take().map(FrontEndDevice::stop);
            let _ = self.camera.stop();
            self.sensor = None;
            return Err(e);
        }
        self.startup.stream_on = t.elapsed();
        self.startup.start = t_start.elapsed();
        self.startup.started_at = Some(Instant::now());
        Ok(())
    }

    /// The next frame: through the back end with the newest settings (see the [module
    /// documentation](self)), its statistics through the algorithms meanwhile, their sensor
    /// request to the control schedule.
    pub fn next(&mut self, timeout: Duration) -> Result<PispFrame> {
        loop {
            if let PispEvent::Frame(f) = self.next_event(timeout, false)? {
                return Ok(f);
            }
        }
    }

    /// As [`Self::next`], and with `slices` (while slicing: [`PispOptions::slice_line`]) a
    /// sliced frame's first rows as soon as they are final ([`PispEvent::Slice`]), before the
    /// frame itself.
    pub fn next_event(&mut self, timeout: Duration, slices: bool) -> Result<PispEvent> {
        let (Some(fe_dev), Some(be_dev), Some(sensor)) = (
            self.fe_dev.as_mut(),
            self.be_dev.as_mut(),
            self.sensor.as_ref(),
        ) else {
            return Err(PipelineError::Device("not started".into()));
        };
        // Settled: the algorithms (and the statistics they read) only every few frames. Decided
        // for the frame expected next, before its statistics are copied out.
        let run = self.algo.due(self.last_seq.map(|s| s + 1));
        let slice = self
            .slicing
            .as_mut()
            .map(|s| (s, &mut *be_dev, &mut self.be));
        if let Waited::Slice(mut s) =
            super::pisp_slices::wait_for_statistics(fe_dev, sensor, timeout, slice, slices)?
        {
            s.raw_image = s.raw_image.filter(|_| self.options.raw_lease);
            return Ok(PispEvent::Slice(s));
        }
        let held =
            fe_dev.next_held_raw(&mut self.fe, timeout, run.then_some(&mut *self.raw_stats))?;
        let dequeued = Instant::now();
        let image = held
            .image
            .ok_or_else(|| PipelineError::Device("no raw frame".into()))?;
        let seq = u64::from(image.sequence);
        profile::time("sensor", "frame_done", || sensor.frame_done(seq));
        let result = (|| {
            let controls = profile::time("sensor", "applied", || sensor.applied(seq))
                .ok_or_else(|| PipelineError::Device(format!("no control values for {seq}")))?;
            let values = sensor_values(seq, &controls);
            let focus = (
                super::lens_state(&controls),
                run.then(|| super::pdaf_grid(&self.controls, seq)).flatten(),
            );
            let mut be = BackEnd {
                dev: be_dev,
                builder: &mut self.be,
                timeout,
                prepare: Duration::ZERO,
                slicing: self.slicing.as_mut(),
            };
            let mut fe = FeStats {
                raw: &self.raw_stats,
                fe: &mut self.fe,
            };
            let (raw_copy, info, last_seq) = (&mut self.raw_copy, &self.info, &mut self.last_seq);
            let mut raw = None;
            let p = self.algo.process(
                &mut be,
                &mut fe,
                &Profiled(&self.controls),
                image.index,
                &values,
                run,
                focus,
                |_, step| {
                    if raw_copy.as_mut().is_some_and(|want| want(&values)) {
                        let data = fe_dev.image_data(image.index).unwrap_or(&[]);
                        raw = super::hold_raw(data, fe_dev.image_format(), info, step, &values)
                            .map(|h| {
                                Box::new(crate::still::HeldRaw {
                                    timestamp: image.timestamp,
                                    ..h
                                })
                            });
                    }
                    *last_seq = Some(seq);
                    // A frame start that came meanwhile (at high rates the next frame starts
                    // about when this one's statistics arrive): the request below knows how
                    // much of it is left.
                    sensor.service();
                },
            )?;
            let be_prepare = p.times.settings + be.prepare;
            if profile::enabled() {
                profile::record("loop", "algorithms", p.times.algorithms);
                profile::record("loop", "be_finish_wait", p.times.finish);
            }
            let t_passes = Instant::now();
            let passes =
                match passes::run(be.dev, &mut self.passes, be.builder, image.index, timeout) {
                    Ok(passes) => passes,
                    Err(e) => {
                        be.dev.release(&p.output);
                        return Err(e);
                    }
                };
            let passes_took = t_passes.elapsed();
            Ok(PispFrame {
                sequence: seq,
                timestamp: image.timestamp,
                dequeued,
                sensor: values,
                job: p.output,
                sequence_mismatch: held.sequence != image.sequence,
                request_lands: p.request_lands,
                settings_from: p.settings_from,
                digital_gain: p.digital_gain,
                flicker: p.flicker,
                times: PispTimes {
                    stats: p.times.stats,
                    algorithms: p.times.algorithms,
                    be_prepare,
                    be_job: p.output.elapsed,
                    total: dequeued.elapsed(),
                    passes: passes_took,
                },
                raw,
                raw_image: self.options.raw_lease.then_some(image.index),
                passes,
            })
        })();
        // Held for its consumer with `raw_lease` (it gives it back with `release_raw`).
        if !(self.options.raw_lease && result.is_ok()) {
            fe_dev.release_image(image.index)?;
        }
        if profile::enabled() {
            profile::record("loop", "dequeued_to_return", dequeued.elapsed());
        }
        result.map(PispEvent::Frame)
    }

    /// Copies the raw frame of every frame `want` accepts (given what produced it) into its
    /// [`PispFrame::raw`] while the back end processes it (`None`: none).
    pub fn set_raw_copy(&mut self, want: Option<super::RawCopy>) {
        self.raw_copy = want;
    }

    /// Sets the stopped camera up for other settings (e.g. another frame rate) without powering
    /// it down: a sensor already in the mode only gets the new frame length. The next
    /// [`Self::start`] reopens the ISP and starts from what the last session settled on.
    pub fn reconfigure(&mut self, settings: &StreamSettings) -> Result<()> {
        if self.sensor.is_some() {
            return Err(PipelineError::Device("stop before reconfiguring".into()));
        }
        let t = Instant::now();
        let configured = self.camera.configure_external(settings)?;
        let fps = configured.interval.fps();
        let info = SensorInfo::from_description(
            &self.camera.info().description,
            &configured.mode.mode,
            &configured.mode.format,
        )?
        .with_flips(
            &self.camera.info().description,
            configured.hflip,
            configured.vflip,
        )?
        .with_fps(fps, fps)?;
        let mut info = info;
        info.camera.temporal_denoise = self.info.camera.temporal_denoise;
        self.algo.controller().set_config(info.camera.clone())?;
        if (info.width, info.height, info.cfa, configured.mode.code)
            != (
                self.info.width,
                self.info.height,
                self.info.cfa,
                self.configured.mode.code,
            )
        {
            self.fe_dev = None;
            self.be_dev = None;
        }
        self.info = info;
        self.configured = configured;
        self.startup.configure = t.elapsed();
        self.startup.bring_up = self.camera.bring_up_times();
        Ok(())
    }

    /// Stops streaming and frees the ISP buffers; the camera stays configured and powered
    /// ([`Self::start`] again, or [`Self::reconfigure`] first, restarts without a bring-up).
    /// What the algorithms settled on is remembered for the camera's next start
    /// ([`crate::warm`]).
    pub fn stop(&mut self) -> Result<()> {
        if self.sensor.is_some()
            && let Some(w) = self.algo.controller_ref().warm_state()
        {
            crate::warm::remember(&self.camera.info().key, w);
        }
        // The front end's STREAMOFF holds its nodes' locks while the bridge waits for the stop
        // acknowledgement: the event thread must not be polling them.
        self.camera.quiesce_external();
        let fe = self.fe_dev.take().map(FrontEndDevice::stop);
        let be = self.be_dev.take().map(BackEndStream::stop);
        self.sensor = None;
        let cam = self.camera.stop();
        fe.transpose()?;
        be.transpose()?;
        cam?;
        Ok(())
    }

    /// Stops and powers the camera down.
    pub fn close(mut self) -> Result<()> {
        let stopped = self.stop();
        let mut camera = self.camera;
        let closed = camera.stop();
        drop(camera);
        stopped.and(closed.map_err(Into::into))
    }
}

/// Records a profile timing when profiling is on.
fn profile_record(what: &'static str, op: &'static str, d: Duration) {
    if profile::enabled() {
        profile::record(what, op, d);
    }
}
