//! The Rockchip ISP30 path on a native camera (RK3588 / RK3588S / RK3582, vendor kernel).
//!
//! ```text
//! sensor ─► rkcif ─► rkisp-isp-subdev ─► rkisp-statistics ─► crate::rkisp::statistics ─► Controller
//!                         ▲     └─► rkisp_mainpath (NV12)                                   │   │
//!                         └──── rkisp-input-params ◄── crate::rkisp::params ◄───────────────┘   └─► SensorRequest
//! ```
//!
//! The ISP processes frames as they arrive (an inline ISP, like the PiSP front end): frame
//! F's statistics go through the algorithms, their sensor request to the control schedule
//! and their ISP settings into the next parameter buffer, which the driver applies to the
//! frame it takes next. The camera runs its sensor side only ([`NativeCamera::configure_external`]);
//! the ISP's main path starts the stream.
//!
//! Status: written against the vendor driver's uAPI; not yet run on a board.

use std::path::Path;
use std::time::Duration;

use styx_algo::{Statistics, Tuning, WarmStart};
use styx_kernel::subdev::MbusCode;
use styx_native::{
    CameraControls, Configured, NativeCamera, SensorStream, StreamSettings, select_mode,
};
use styx_rkisp::device::{IspDevice, IspSetup, OutputFrame, find_isps};

use super::sensor_values;
use crate::controller::{Controller, SensorValues};
use crate::error::{PipelineError, Result};
use crate::process::Algorithms;
use crate::rkisp::{params, statistics};
use crate::sensor::{ISSUE_LATENCY, SensorInfo};

impl From<styx_rkisp::device::DeviceError> for PipelineError {
    fn from(e: styx_rkisp::device::DeviceError) -> Self {
        PipelineError::Device(format!("rkisp: {e}"))
    }
}

/// Buffers per queue (parameters, statistics, output).
pub const RKISP_BUFFERS: u32 = 4;

/// One frame through the ISP.
#[derive(Debug)]
pub struct RkispFrame {
    /// The main path's frame (NV12): its bytes from [`RkispPipeline::data`], back to the
    /// driver with [`RkispPipeline::release`].
    pub output: OutputFrame,
    /// What produced it.
    pub sensor: SensorValues,
    /// Frame the latest new sensor request lands on, if this frame made one.
    pub request_lands: Option<u64>,
}

/// A native camera through the Rockchip ISP. See the [module documentation](self).
pub struct RkispPipeline {
    camera: NativeCamera,
    controls: CameraControls,
    configured: Configured,
    dev: Option<IspDevice>,
    algo: Algorithms,
    info: SensorInfo,
    sensor: Option<SensorStream>,
    stats: Statistics,
    warm_override: Option<Option<WarmStart>>,
}

impl std::fmt::Debug for RkispPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RkispPipeline")
            .field("configured", &self.configured)
            .finish_non_exhaustive()
    }
}

/// Whether the system has a Rockchip ISP30 (its media device, from the vendor driver).
pub fn has_rkisp() -> bool {
    !find_isps().is_empty()
}

impl RkispPipeline {
    /// Opens `camera` for `settings` (a raw mode) through the first ISP of
    /// [`find_isps`](styx_rkisp::device::find_isps), its main path giving `output`-sized NV12,
    /// with the algorithms of `tuning`.
    pub fn open(
        camera: NativeCamera,
        settings: &StreamSettings,
        tuning: &Tuning,
        output: (u32, u32),
    ) -> Result<Self> {
        let media = find_isps()
            .into_iter()
            .next()
            .ok_or_else(|| PipelineError::Device("no rkisp media device".into()))?;
        Self::open_on(&media, camera, settings, tuning, output)
    }

    /// [`Self::open`] through the ISP of media device `media`.
    pub fn open_on(
        media: &Path,
        mut camera: NativeCamera,
        settings: &StreamSettings,
        tuning: &Tuning,
        output: (u32, u32),
    ) -> Result<Self> {
        let mode = select_mode(&camera.info().modes, settings, &camera.info().raw_formats)?.clone();
        let code = camera.bus_code(mode.code, settings.hflip, settings.vflip);
        let configured = camera.configure_external(settings)?;
        let dev = IspDevice::open(
            media,
            &IspSetup {
                width: configured.mode.width,
                height: configured.mode.height,
                sensor_code: MbusCode(code),
                output,
                buffers: RKISP_BUFFERS,
            },
        )?;
        let fps = configured.interval.fps();
        let desc = std::sync::Arc::clone(&camera.info().description);
        let mut info =
            SensorInfo::from_description(&desc, &configured.mode.mode, &configured.mode.format)?
                .with_flips(&desc, configured.hflip, configured.vflip)?
                .with_fps(fps, fps)?;
        info.camera.lens = super::lens_config(&camera);
        let controller = Controller::new(tuning, info.camera.clone())?;
        let controls = camera.controls();
        Ok(Self {
            controls,
            configured,
            dev: Some(dev),
            algo: Algorithms::new(controller, info.black_level),
            info,
            camera,
            sensor: None,
            stats: Statistics::default(),
            warm_override: None,
        })
    }

    /// The configuration in effect.
    pub fn configured(&self) -> &Configured {
        &self.configured
    }

    /// The camera.
    pub fn camera(&self) -> &NativeCamera {
        &self.camera
    }

    /// The camera's controls.
    pub fn controls(&self) -> &CameraControls {
        &self.controls
    }

    /// The sensor as the algorithms see it.
    pub fn info(&self) -> &SensorInfo {
        &self.info
    }

    /// The algorithms.
    pub fn algorithms(&mut self) -> &mut Algorithms {
        &mut self.algo
    }

    /// The main path's output size and line stride.
    pub fn output_layout(&self) -> Option<((u32, u32), u32)> {
        self.dev.as_ref().map(|d| (d.output_size(), d.stride()))
    }

    /// Starts the next [`Self::start`] (only) from these settled values (`None`: from the
    /// tuning's start-up values) instead of what the camera's last session settled on.
    pub fn set_warm_start(&mut self, warm: Option<WarmStart>) {
        self.warm_override = Some(warm);
    }

    /// Resets the algorithms, requests their start-up exposure for frame 0, queues the
    /// start-up parameters and starts the ISP (and with it the sensor).
    pub fn start(&mut self) -> Result<()> {
        let dev = self
            .dev
            .as_mut()
            .ok_or_else(|| PipelineError::Device("closed".into()))?;
        let sensor = self.camera.start_external(dev.output_path())?;
        self.algo.controller().set_issue_latency(ISSUE_LATENCY);
        self.controls.set_loop_latency(Some(ISSUE_LATENCY));
        let warm = match self.warm_override.take() {
            Some(w) => w,
            None => crate::warm::recall(&self.camera.info().key),
        };
        self.algo.controller().set_warm_start(warm);
        let (w, h) = (self.info.width, self.info.height);
        let started = self
            .algo
            .start(&self.controls)
            .and_then(|s| Ok(dev.start(&params(&s.isp, 0, w, h))?))
            .and_then(|()| Ok(self.camera.resume_external()?));
        if let Err(e) = started {
            self.camera.quiesce_external();
            let _ = self.dev.take().map(IspDevice::close);
            let _ = self.camera.stop();
            return Err(e);
        }
        self.sensor = Some(sensor);
        Ok(())
    }

    /// The next frame: its statistics through the algorithms (when they are due), their
    /// settings queued for the frames the ISP takes next, then the main path's frame.
    pub fn next(&mut self, timeout: Duration) -> Result<RkispFrame> {
        let (dev, sensor) = match (self.dev.as_mut(), self.sensor.as_ref()) {
            (Some(d), Some(s)) => (d, s),
            _ => return Err(PipelineError::Device("not started".into())),
        };
        let (w, h) = (self.info.width, self.info.height);
        let stats = &mut self.stats;
        let (seq, _, measured) = dev.next_statistics(timeout, |b| statistics(b, w, h, stats))?;
        let seq = u64::from(seq);
        sensor.frame_done(seq);
        let controls = sensor
            .applied(seq)
            .or_else(|| self.controls.applied(seq))
            .ok_or_else(|| PipelineError::Device(format!("no control values for frame {seq}")))?;
        let values = sensor_values(seq, &controls);
        let request_lands = if measured && self.algo.due(Some(seq)) {
            self.algo.run(&self.stats, &values, None, &self.controls)?
        } else {
            None
        };
        let settings = self.algo.settings(&values);
        dev.queue_params(&params(settings, seq as u32 + 1, w, h))?;
        let output = dev.next_output(timeout)?;
        Ok(RkispFrame {
            output,
            sensor: values,
            request_lands,
        })
    }

    /// A frame's NV12 bytes.
    pub fn data(&self, frame: &RkispFrame) -> &[u8] {
        self.dev
            .as_ref()
            .map_or(&[][..], |d| d.output_data(&frame.output))
    }

    /// Gives a frame's buffer back to the ISP.
    pub fn release(&self, frame: &RkispFrame) -> Result<()> {
        if let Some(d) = &self.dev {
            d.release(&frame.output)?;
        }
        Ok(())
    }

    fn remember(&mut self) {
        if self.sensor.is_some()
            && let Some(w) = self.algo.controller().warm_state()
        {
            crate::warm::remember(&self.camera.info().key, w);
        }
    }

    /// Stops the ISP and the camera and powers it down; what the algorithms settled on is
    /// remembered for the camera's next start.
    pub fn close(mut self) -> Result<()> {
        self.remember();
        self.camera.quiesce_external();
        let isp = self.dev.take().map_or(Ok(()), IspDevice::close);
        self.sensor = None;
        let cam = self.camera.close();
        isp?;
        Ok(cam?)
    }
}
