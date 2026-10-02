//! The PiSP path on a native camera.
//!
//! ```text
//! sensor ─► csi2 ─► pisp-fe ─► fe_stats  (every frame) ─► stats::from_pisp ─► Controller
//!                      │                                                      │   │
//!                      └─► fe_image0 (16-bit raw, held) ═dma-buf═► pispbe ◄───┘   └─► SensorRequest
//!                                                                   ├─► output0 (e.g. NV12 1280x800)
//!                                                                   └─► output1 (e.g. RGB 640x400)
//! ```
//!
//! The statistics of frame F reach the algorithms before F goes through the back end, so the
//! back end processes F with the white balance, CCM, gamma and digital gain computed from F
//! itself. The front end's RGB-to-Y weights and black levels follow on the next config it
//! takes (configs are queued a couple of frames ahead).

use std::time::{Duration, Instant};

use styx_algo::{Statistics, Tuning, WarmStart};
use styx_kernel::subdev::MbusCode;
use styx_native::{
    CameraControls, Configured, NativeCamera, SensorStream, StreamSettings, select_mode,
};
use styx_pisp::be::BackEnd;
use styx_pisp::device::{
    BackEndStream, BeFormat, BeJob, BeOutputSetup, FrontEndDevice, FrontEndSetup,
};
use styx_pisp::fe::FrontEnd;
use styx_pisp::uapi::{BayerOrder, ImageFormatConfig, fe_enable};
use styx_softisp::CfaPattern;

use super::{apply_request, sensor_values};
use crate::controller::{Controller, SensorValues, Step};
use crate::error::{PipelineError, Result};
use crate::isp::{be_template, level16};
use crate::sensor::{ISSUE_LATENCY, SensorInfo};
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
        }
    }
}

/// Where one frame's time went.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PispTimes {
    /// Running the algorithms.
    pub algorithms: Duration,
    /// Building the back end config and its tiles.
    pub be_prepare: Duration,
    /// The back end job (config queued to output dequeued).
    pub be_job: Duration,
    /// From the front end's buffers being dequeued to the outputs being ready.
    pub total: Duration,
}

/// Where opening and starting the PiSP path spent its time.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PispStartup {
    /// Sensor and bridge set-up ([`NativeCamera::configure_external`]): power, chip id, init and
    /// mode registers, bridge format and timing.
    pub configure: Duration,
    /// The sensor bring-up within `configure`.
    pub bring_up: styx_native::BringUpTimes,
    /// Front end: links, formats, buffers.
    pub fe_open: Duration,
    /// Back end: formats, buffers, the front end's buffers imported.
    pub be_open: Duration,
    /// Back end template and the algorithms.
    pub isp_and_algorithms: Duration,
    /// Everything [`PispPipeline::open`] took.
    pub open: Duration,
    /// Algorithms' start-up values and the sensor request for frame 0.
    pub start_values: Duration,
    /// Embedded data capture and the event thread.
    pub start_external: Duration,
    /// The front end's `STREAMON`: the receiver starts, the bridge asks and the sensor starts.
    pub stream_on: Duration,
    /// Everything [`PispPipeline::start`] took.
    pub start: Duration,
    /// When `start` returned (for measuring the first frame from there).
    pub started_at: Option<Instant>,
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
    /// Its statistics.
    pub stats: Statistics,
    /// The loop's output (the settings the back end used are `step.isp`).
    pub step: Step,
    /// The back end job: hand it to [`PispPipeline::output`] and [`PispPipeline::release`].
    pub job: BeJob,
    /// The statistics buffer's sequence differed from the raw frame's.
    pub sequence_mismatch: bool,
    /// Frame the new sensor request lands on, if this frame made one.
    pub request_lands: Option<u64>,
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

/// The front and back end, opened for a sensor mode.
struct Isp {
    fe: FrontEnd,
    fe_dev: FrontEndDevice,
    be_dev: BackEndStream,
    be: BackEnd,
}

impl Isp {
    /// Front end (links, formats, buffers), back end (formats, buffers, the front end's raw
    /// buffers imported) and the back end template, for `info`'s size and colour order on
    /// media bus code `code`. Returns how long each end took.
    fn open(
        info: &SensorInfo,
        code: u32,
        options: &PispOptions,
    ) -> Result<(Self, Duration, Duration)> {
        let t = Instant::now();
        let order = bayer(info.cfa);
        let fe_dev = FrontEndDevice::open(&FrontEndSetup {
            width: info.width,
            height: info.height,
            sensor_code: MbusCode(code),
            bayer: order,
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
        fe.enable(fe_enable::OUTPUT0, true);
        let input_len = input.stride as u32 * u32::from(input.height);
        let be_dev = BackEndStream::open(
            options.be_group,
            input,
            order,
            fe_dev.image_dmabufs()?,
            input_len,
            options.outputs,
            options.be_buffers,
        )?;
        let be = be_template(
            input,
            order,
            info.black_level,
            [be_dev.output_format(0), be_dev.output_format(1)],
        )?;
        // The template must prepare (sizes, strides, tiles) before streaming starts.
        be.clone()
            .prepare()
            .map_err(|e| PipelineError::Config(format!("back end: {}", e.0)))?;
        Ok((
            Self {
                fe,
                fe_dev,
                be_dev,
                be,
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
    controller: Controller,
    fe: FrontEnd,
    fe_dev: Option<FrontEndDevice>,
    be_dev: Option<BackEndStream>,
    be: BackEnd,
    sensor: Option<SensorStream>,
    options: PispOptions,
    startup: PispStartup,
    warm_override: Option<Option<WarmStart>>,
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
        let mut startup = PispStartup::default();
        let mode = select_mode(&camera.info().modes, settings, &camera.info().raw_formats)?.clone();
        let desc = std::sync::Arc::clone(&camera.info().description);
        let sensor_info = SensorInfo::from_description(&desc, &mode.mode, &mode.format)?;
        let (configured, isp) = std::thread::scope(|scope| {
            let sensor = scope.spawn(|| {
                let t = Instant::now();
                let c = camera.configure_external(settings);
                (c, t.elapsed())
            });
            let isp = Isp::open(&sensor_info, mode.code, &options);
            let (configured, took) = sensor.join().unwrap_or_else(|_| {
                (
                    Err(styx_native::NativeError::State("sensor set-up panicked")),
                    Duration::ZERO,
                )
            });
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
        let info =
            SensorInfo::from_description(&desc, &configured.mode.mode, &configured.mode.format)?
                .with_fps(fps, fps)?;
        let controller = Controller::new(tuning, info.camera.clone())?;
        let controls = camera.controls();
        startup.isp_and_algorithms = t.elapsed();
        startup.open = t_open.elapsed();
        Ok(Self {
            camera,
            controls,
            configured,
            info,
            controller,
            fe: isp.fe,
            fe_dev: Some(isp.fe_dev),
            be_dev: Some(isp.be_dev),
            be: isp.be,
            sensor: None,
            options,
            startup,
            warm_override: None,
        })
    }

    /// Where opening and the last start spent their time.
    pub fn startup(&self) -> &PispStartup {
        &self.startup
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

    /// Back end output `i`'s format.
    pub fn output_format(&self, i: usize) -> Option<ImageFormatConfig> {
        self.be_dev.as_ref()?.output_format(i)
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
            let (isp, fe_open, be_open) =
                Isp::open(&self.info, self.configured.mode.code, &self.options)?;
            (self.fe, self.be) = (isp.fe, isp.be);
            self.fe_dev = Some(isp.fe_dev);
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
        let sensor = self.camera.start_external(&sync)?;
        self.startup.start_external = t_ext.elapsed();
        let t = Instant::now();
        let latency = if sensor.uses_frame_sync() {
            self.info
                .issue_latency(PISP_PROCESSING, styx_native::control::DEFAULT_WRITE_MARGIN)
        } else {
            ISSUE_LATENCY
        };
        self.sensor = Some(sensor);
        self.controller.set_issue_latency(latency);
        let warm = match self.warm_override.take() {
            Some(w) => w,
            None => crate::warm::recall(&self.camera.info().key),
        };
        self.controller.set_warm_start(warm);
        let values = self.controller.start().and_then(|start| {
            if let Some(r) = start.sensor {
                apply_request(&self.controls, &r)?;
            }
            start.isp.apply_fe(&mut self.fe);
            Ok(())
        });
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

    /// The next frame: statistics to the algorithms, the sensor request to the control
    /// schedule, the raw frame through the back end with this frame's settings.
    pub fn next(&mut self, timeout: Duration) -> Result<PispFrame> {
        let (Some(fe_dev), Some(be_dev), Some(sensor)) = (
            self.fe_dev.as_mut(),
            self.be_dev.as_mut(),
            self.sensor.as_ref(),
        ) else {
            return Err(PipelineError::Device("not started".into()));
        };
        let held = fe_dev.next_held(&mut self.fe, timeout)?;
        let dequeued = Instant::now();
        let image = held
            .image
            .ok_or_else(|| PipelineError::Device("no raw frame".into()))?;
        let seq = u64::from(image.sequence);
        sensor.frame_done(seq);
        let result = (|| {
            let controls = sensor
                .applied(seq)
                .ok_or_else(|| PipelineError::Device(format!("no control values for {seq}")))?;
            let values = sensor_values(seq, &controls);
            let stats = stats::from_pisp(&held.stats);
            let t0 = Instant::now();
            let step = self.controller.process(&stats, &values)?;
            let request_lands = match &step.sensor {
                Some(r) => Some(apply_request(&self.controls, r)?),
                None => None,
            };
            let algorithms = t0.elapsed();
            step.isp.apply_fe(&mut self.fe);
            let t1 = Instant::now();
            let mut be = self.be.clone();
            step.isp.apply_be(&mut be);
            let cfg = be
                .prepare()
                .map_err(|e| PipelineError::Config(format!("back end: {}", e.0)))?;
            let be_prepare = t1.elapsed();
            let job = be_dev.process(image.index, &cfg, timeout)?;
            Ok(PispFrame {
                sequence: seq,
                timestamp: image.timestamp,
                dequeued,
                sensor: values,
                stats,
                step,
                job,
                sequence_mismatch: held.sequence != image.sequence,
                request_lands,
                times: PispTimes {
                    algorithms,
                    be_prepare,
                    be_job: job.elapsed,
                    total: dequeued.elapsed(),
                },
            })
        })();
        fe_dev.release_image(image.index)?;
        result
    }

    /// Output `i`'s bytes for a frame's job.
    pub fn output(&self, i: usize, job: &BeJob) -> Option<&[u8]> {
        self.be_dev.as_ref()?.output_data(i, job.outputs[i]?)
    }

    /// Output `i`'s dma-buf for a frame's job.
    pub fn output_dmabuf(&self, i: usize, job: &BeJob) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.be_dev.as_ref()?.output_dmabuf(i, job.outputs[i]?)
    }

    /// Gives a frame's output buffers back.
    pub fn release(&mut self, job: &BeJob) {
        if let Some(b) = self.be_dev.as_mut() {
            b.release(job);
        }
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
        .with_fps(fps, fps)?;
        self.controller.set_config(info.camera.clone())?;
        if (info.width, info.height, configured.mode.code)
            != (self.info.width, self.info.height, self.configured.mode.code)
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
            && let Some(w) = self.controller.warm_state()
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
