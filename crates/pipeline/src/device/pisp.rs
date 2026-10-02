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
//! back end config patched only where the settings changed ([`BeConfigBuilder`]). F's
//! statistics go through the algorithms at the start of the next [`PispPipeline::next`],
//! while the pipeline would otherwise only wait for F + 1, so their run time (up to 0.7 ms on
//! the frames AWB and lens shading run) never delays a frame. Sensor requests still name the
//! frame they land on and are issued in the same frame period. The order of inputs and
//! outputs is fixed, so the loop stays deterministic. The front end's RGB-to-Y weights and
//! black levels follow on the next config it takes (configs are queued a couple of frames
//! ahead).

use std::time::{Duration, Instant};

use styx_algo::{Statistics, Tuning};
use styx_kernel::subdev::MbusCode;
use styx_native::{CameraControls, Configured, NativeCamera, SensorStream, StreamSettings};
use styx_pisp::device::{
    BackEndStream, BeFormat, BeJob, BeOutputSetup, FrontEndDevice, FrontEndSetup, OutputMemory,
    profile,
};
use styx_pisp::fe::FrontEnd;
use styx_pisp::uapi::{BayerOrder, ImageFormatConfig, RawStatistics, fe_enable};
use styx_softisp::CfaPattern;

use super::{apply_request, sensor_values};
use crate::controller::{Controller, SensorValues, Step};
use crate::error::{PipelineError, Result};
use crate::isp::{IspSettings, be_template, level16};
use crate::pisp_be::{BeConfigBuilder, BeUpdateCounts};
use crate::sensor::SensorInfo;
use crate::stats;

/// How the PiSP path is set up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PispOptions {
    /// Back end outputs: format and size (output 1 has the downscaler).
    pub outputs: [Option<BeOutputSetup>; 2],
    /// Front end buffers per queue (raw frames, statistics, configs).
    pub fe_buffers: u32,
    /// Back end buffers per output.
    pub be_buffers: u32,
    /// Back end node group (0 or 1).
    pub be_group: usize,
    /// Front end configs queued ahead of the frames.
    pub configs_ahead: usize,
    /// Where the back end's output buffers come from: a cached dma-heap (the default: CPU
    /// reads at memory speed, bracketed by [`PispPipeline::sync_output`]) or the driver's
    /// (mapped uncached).
    pub output_memory: OutputMemory,
}

impl PispOptions {
    /// NV12 at the sensor size on output 0 and RGB24 at half size on output 1.
    pub fn nv12_and_half_rgb(width: u32, height: u32) -> Self {
        Self {
            outputs: [
                Some(BeOutputSetup {
                    format: BeFormat::Nv12,
                    width,
                    height,
                }),
                Some(BeOutputSetup {
                    format: BeFormat::Rgb24,
                    width: width / 2,
                    height: height / 2,
                }),
            ],
            fe_buffers: 6,
            be_buffers: 4,
            be_group: 0,
            configs_ahead: 2,
            output_memory: OutputMemory::CachedHeap,
        }
    }
}

/// Where one frame's time went.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PispTimes {
    /// Converting the statistics for the algorithms.
    pub stats: Duration,
    /// Running the algorithms on the previous frame's statistics (before this frame arrived,
    /// off the frame path).
    pub algorithms: Duration,
    /// Updating the back end config (and its tiles, when they change).
    pub be_prepare: Duration,
    /// The back end job (config queued to output dequeued).
    pub be_job: Duration,
    /// From the front end's buffers being dequeued to the outputs being ready.
    pub total: Duration,
}

/// One frame through the PiSP.
#[derive(Debug)]
pub struct PispFrame {
    /// Frame sequence.
    pub sequence: u64,
    /// Capture timestamp (`CLOCK_MONOTONIC`, frame start on `rp1-cfe`).
    pub timestamp: Duration,
    /// When the front end's buffers were dequeued.
    pub dequeued: Instant,
    /// What produced the frame.
    pub sensor: SensorValues,
    /// The back end job: hand it to [`PispPipeline::output`] and [`PispPipeline::release`].
    pub job: BeJob,
    /// The statistics buffer's sequence differed from the raw frame's.
    pub sequence_mismatch: bool,
    /// Frame the sensor request made in this call lands on (from the previous frame's
    /// statistics), if one was made.
    pub request_lands: Option<u64>,
    /// The frame whose statistics the back end settings came from (`None` before the
    /// algorithms have seen a frame). The settings are [`PispPipeline::step`]'s `isp`.
    pub settings_from: Option<u64>,
    /// Time spent.
    pub times: PispTimes,
}

fn bayer(c: CfaPattern) -> BayerOrder {
    match c {
        CfaPattern::Rggb => BayerOrder::Rggb,
        CfaPattern::Bggr => BayerOrder::Bggr,
        CfaPattern::Grbg => BayerOrder::Grbg,
        CfaPattern::Gbrg => BayerOrder::Gbrg,
    }
}

/// A native camera through the PiSP. See the [module documentation](self).
pub struct PispPipeline {
    camera: NativeCamera,
    controls: CameraControls,
    configured: Configured,
    info: SensorInfo,
    controller: Controller,
    fe: FrontEnd,
    fe_dev: Option<FrontEndDevice>,
    be_dev: Option<BackEndStream>,
    be: BeConfigBuilder,
    sensor: Option<SensorStream>,
    options: PispOptions,
    /// The last frame's statistics, as copied from the front end and as converted.
    raw_stats: Box<RawStatistics>,
    stats: Statistics,
    /// What produced the frame whose statistics are in `stats` and await the algorithms.
    pending: Option<SensorValues>,
    /// The newest output of the algorithms (its `isp` processes the frames), with the frame
    /// it came from (`None`: the start-up settings).
    step: Step,
    stepped: bool,
}

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
    /// rate) with the algorithms of `tuning`.
    pub fn open(
        mut camera: NativeCamera,
        settings: &StreamSettings,
        tuning: &Tuning,
        options: PispOptions,
    ) -> Result<Self> {
        let configured = camera.configure_external(settings)?;
        let fps = configured.interval.fps();
        let info = SensorInfo::from_description(
            &camera.info().description,
            &configured.mode.mode,
            &configured.mode.format,
        )?
        .with_fps(fps, fps)?;
        let order = bayer(info.cfa);
        let fe_dev = FrontEndDevice::open(&FrontEndSetup {
            width: info.width,
            height: info.height,
            sensor_code: MbusCode(configured.mode.code),
            bayer: order,
            image_output: true,
            buffers: options.fe_buffers,
            keep_embedded: true,
        })?;
        let input = fe_dev.image_format();
        let mut fe = FrontEnd::new(info.width as u16, info.height as u16, order);
        fe.default_stats(level16(info.black_level), 1.0, 1.0);
        fe.set_output_format(0, input);
        fe.enable(fe_enable::OUTPUT0, true);
        let input_len = input.stride as u32 * u32::from(input.height);
        let be_dev = BackEndStream::open_with(
            options.be_group,
            input,
            order,
            fe_dev.image_dmabufs()?,
            input_len,
            options.outputs,
            options.be_buffers,
            options.output_memory,
        )?;
        let be = be_template(
            input,
            order,
            info.black_level,
            [be_dev.output_format(0), be_dev.output_format(1)],
        )?;
        // The template must prepare (sizes, strides, tiles) before streaming starts.
        let be = BeConfigBuilder::new(be)?;
        let controller = Controller::new(tuning, info.camera.clone())?;
        let controls = camera.controls();
        let info_black = info.black_level;
        Ok(Self {
            camera,
            controls,
            configured,
            info,
            controller,
            fe,
            fe_dev: Some(fe_dev),
            be_dev: Some(be_dev),
            be,
            sensor: None,
            options,
            raw_stats: bytemuck::allocation::zeroed_box(),
            stats: Statistics::default(),
            pending: None,
            step: Step {
                frame: 0,
                sensor: None,
                isp: IspSettings::neutral(info_black),
                params: Default::default(),
            },
            stepped: false,
        })
    }

    /// The configuration in effect.
    pub fn configured(&self) -> &Configured {
        &self.configured
    }

    /// The sensor mode.
    pub fn info(&self) -> &SensorInfo {
        &self.info
    }

    /// The controller (controls, recording).
    pub fn controller(&mut self) -> &mut Controller {
        &mut self.controller
    }

    /// The camera's controls.
    pub fn controls(&self) -> &CameraControls {
        &self.controls
    }

    /// The statistics of the frame [`Self::next`] returned last, as the algorithms see them.
    pub fn statistics(&self) -> &Statistics {
        &self.stats
    }

    /// The algorithms' newest output: its `isp` settings (with the digital gain for the frame
    /// [`Self::next`] returned last) processed that frame; `step.frame` is the frame whose
    /// statistics produced it ([`PispFrame::settings_from`]).
    pub fn step(&self) -> &Step {
        &self.step
    }

    /// How often the back end config was rebuilt, patched or reused.
    pub fn be_updates(&self) -> BeUpdateCounts {
        self.be.counts()
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

    /// Resets the algorithms, requests their start-up values for frame 0 and starts streaming.
    pub fn start(&mut self) -> Result<()> {
        let start = self.controller.start()?;
        if let Some(r) = start.sensor {
            apply_request(&self.controls, &r)?;
        }
        start.isp.apply_fe(&mut self.fe);
        self.step = Step {
            frame: 0,
            sensor: start.sensor,
            isp: start.isp,
            params: Default::default(),
        };
        self.stepped = false;
        self.pending = None;
        let fe_dev = self
            .fe_dev
            .as_mut()
            .ok_or_else(|| PipelineError::Device("stopped".into()))?;
        let sync = fe_dev
            .image_node_path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| PipelineError::Device("no fe_image0 node".into()))?;
        self.sensor = Some(self.camera.start_external(&sync)?);
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
        Ok(())
    }

    /// Runs the algorithms on the statistics waiting for them (the previous frame's) and
    /// hands their sensor request to the control schedule.
    fn run_algorithms(&mut self) -> Result<(Option<u64>, Duration)> {
        let Some(values) = self.pending.take() else {
            return Ok((None, Duration::ZERO));
        };
        let t = Instant::now();
        let step = profile::time("loop", "algorithms", || {
            self.controller.process(&self.stats, &values)
        })?;
        let lands = match &step.sensor {
            Some(r) => Some(profile::time("sensor", "request", || {
                apply_request(&self.controls, r)
            })?),
            None => None,
        };
        step.isp.apply_fe(&mut self.fe);
        self.step = step;
        self.stepped = true;
        Ok((lands, t.elapsed()))
    }

    /// The next frame: the previous frame's statistics through the algorithms (their sensor
    /// request to the control schedule), then this frame through the back end with the
    /// newest settings (see the [module documentation](self)).
    pub fn next(&mut self, timeout: Duration) -> Result<PispFrame> {
        let (request_lands, algorithms) = self.run_algorithms()?;
        let (Some(fe_dev), Some(be_dev), Some(sensor)) = (
            self.fe_dev.as_mut(),
            self.be_dev.as_mut(),
            self.sensor.as_ref(),
        ) else {
            return Err(PipelineError::Device("not started".into()));
        };
        let held = fe_dev.next_held_raw(&mut self.fe, timeout, &mut self.raw_stats)?;
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
            let t1 = Instant::now();
            // The newest settings, with the digital gain for what this frame got.
            if self.stepped {
                let g = self.step.params.colour_gains[1].max(1e-6);
                self.step.isp.digital_gain =
                    self.controller.digital_gain_for(&self.step.params, &values) * g;
            }
            profile::time("loop", "be_update", || self.be.update(&self.step.isp))?;
            let be_prepare = t1.elapsed();
            let job = be_dev.process_queued(image.index, self.be.config())?;
            // While the back end works: the statistics for the algorithms' next run.
            let ts = Instant::now();
            stats::from_pisp_raw(&self.raw_stats, &mut self.stats);
            self.pending = Some(values);
            let stats_time = ts.elapsed();
            let job = be_dev.wait_job(job, timeout)?;
            Ok(PispFrame {
                sequence: seq,
                timestamp: image.timestamp,
                dequeued,
                sensor: values,
                job,
                sequence_mismatch: held.sequence != image.sequence,
                request_lands,
                settings_from: self.stepped.then_some(self.step.frame),
                times: PispTimes {
                    stats: stats_time,
                    algorithms,
                    be_prepare,
                    be_job: job.elapsed,
                    total: dequeued.elapsed(),
                },
            })
        })();
        fe_dev.release_image(image.index)?;
        if profile::enabled() {
            profile::record("loop", "dequeued_to_return", dequeued.elapsed());
        }
        result
    }

    /// Output `i`'s bytes for a frame's job.
    pub fn output(&self, i: usize, job: &BeJob) -> Option<&[u8]> {
        self.be_dev.as_ref()?.output_data(i, job.outputs[i]?)
    }

    /// Brackets CPU reads of output `i` of a frame's job: call with `start` before reading
    /// [`Self::output`] and without after (see [`BackEndStream::sync_output`]).
    pub fn sync_output(&self, i: usize, job: &BeJob, start: bool) -> Result<()> {
        let (Some(b), Some(index)) = (self.be_dev.as_ref(), job.outputs.get(i).copied().flatten())
        else {
            return Ok(());
        };
        Ok(b.sync_output(i, index, start)?)
    }

    /// Output `i`'s dma-buf for a frame's job.
    pub fn output_dmabuf(&self, i: usize, job: &BeJob) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.be_dev.as_ref()?.output_dmabuf(i, job.outputs[i]?)
    }

    /// Gives output `i`'s buffer `index` back (one output of a job, e.g. when its two outputs
    /// go to different consumers).
    pub fn release_output(&mut self, i: usize, index: u32) {
        if let Some(b) = self.be_dev.as_mut() {
            b.release_output(i, index);
        }
    }

    /// Gives a frame's output buffers back.
    pub fn release(&mut self, job: &BeJob) {
        if let Some(b) = self.be_dev.as_mut() {
            b.release(job);
        }
    }

    /// Stops streaming and frees the ISP buffers; the camera stays configured and powered.
    pub fn stop(&mut self) -> Result<()> {
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
