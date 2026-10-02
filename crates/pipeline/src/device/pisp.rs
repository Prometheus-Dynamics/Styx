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

use styx_algo::{Statistics, Tuning};
use styx_kernel::subdev::MbusCode;
use styx_native::{CameraControls, Configured, NativeCamera, SensorStream, StreamSettings};
use styx_pisp::be::BackEnd;
use styx_pisp::device::{
    BackEndStream, BeFormat, BeJob, BeOutputSetup, FrontEndDevice, FrontEndSetup,
};
use styx_pisp::fe::FrontEnd;
use styx_pisp::uapi::{
    BayerOrder, BeOutputFormatConfig, ImageFormatConfig, fe_enable, image_format, rgb_enable,
};
use styx_softisp::CfaPattern;

use super::{apply_request, sensor_values};
use crate::controller::{Controller, SensorValues, Step};
use crate::error::{PipelineError, Result};
use crate::isp::level16;
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

/// The back end config every frame starts from: the fixed Bayer pipeline (black level, white
/// balance, demosaic, CCM, sharpening, false colour, gamma) and both outputs at their sizes,
/// with the full-range BT.601 conversion on YUV outputs.
pub fn be_template(
    input: ImageFormatConfig,
    order: BayerOrder,
    black_level: f64,
    outputs: [Option<ImageFormatConfig>; 2],
) -> Result<BackEnd> {
    let out0 = outputs[0].ok_or_else(|| PipelineError::Config("no output 0".into()))?;
    let mut be = BackEnd::simple_bayer(
        input,
        order,
        level16(black_level),
        (1.0, 1.0, 1.0),
        None,
        out0.format,
    );
    let yuv = |f: u32| f & (image_format::SAMPLING_MASK | image_format::PLANARITY_MASK) != 0;
    let jpeg = styx_pisp::be::defaults::encoding("jpeg").expect("jpeg encoding");
    for (i, o) in outputs.iter().enumerate() {
        let Some(o) = o else { continue };
        be.set_output_format(
            i,
            BeOutputFormatConfig {
                image: *o,
                ..Default::default()
            },
        );
        if (o.width, o.height) != (input.width, input.height) {
            be.set_smart_resize(i, o.width, o.height);
        }
        let mut rgb = be.config().global.rgb_enables | rgb_enable::output(i);
        if yuv(o.format) {
            be.set_csc(i, jpeg.ycbcr);
            rgb |= rgb_enable::csc(i);
        }
        let g = be.config().global;
        be.set_global(g.bayer_enables, rgb, order);
    }
    Ok(be)
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
        let controller = Controller::new(tuning, info.camera.clone())?;
        let controls = camera.controls();
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

    /// Resets the algorithms, requests their start-up values for frame 0 and starts streaming.
    pub fn start(&mut self) -> Result<()> {
        let start = self.controller.start()?;
        if let Some(r) = start.sensor {
            apply_request(&self.controls, &r)?;
        }
        start.isp.apply_fe(&mut self.fe);
        let fe_dev = self
            .fe_dev
            .as_mut()
            .ok_or_else(|| PipelineError::Device("stopped".into()))?;
        let sync = fe_dev.image_node_path().map(|p| p.to_path_buf());
        self.sensor = Some(self.camera.start_external(sync.as_deref())?);
        if let Err(e) = fe_dev.start(&mut self.fe, self.options.configs_ahead) {
            let _ = self.camera.stop();
            self.sensor = None;
            return Err(e.into());
        }
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

    /// Stops streaming and frees the ISP buffers; the camera stays configured and powered.
    pub fn stop(&mut self) -> Result<()> {
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
