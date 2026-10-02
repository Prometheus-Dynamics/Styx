//! The software ISP path on a native camera: packed raw frames from the receiver's raw node.

use std::time::Duration;

use styx_algo::Tuning;
use styx_native::{
    CameraControls, CameraInfo, Configured, FrameStream, NativeCamera, NativeFrame, StreamSettings,
};
use styx_softisp::{OutputBuffers, RawFormat, Scale};

use super::{apply_request, sensor_values};
use crate::controller::SensorValues;
use crate::error::{PipelineError, Result};
use crate::sensor::SensorInfo;
use crate::soft::{SoftLoop, SoftOutput};

/// One frame through the software path.
#[derive(Debug)]
pub struct SoftFrame {
    /// The raw frame (its buffer goes back to the queue when dropped).
    pub raw: NativeFrame,
    /// What produced it.
    pub sensor: SensorValues,
    /// The loop's output.
    pub output: SoftOutput,
    /// Frame the latest new sensor request lands on, if this frame made one.
    pub request_lands: Option<u64>,
}

/// A native camera with the software ISP loop. See the [module documentation](self).
pub struct SoftPipeline {
    camera: NativeCamera,
    controls: CameraControls,
    stream: Option<FrameStream>,
    soft: SoftLoop,
    configured: Configured,
}

impl std::fmt::Debug for SoftPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoftPipeline")
            .field("configured", &self.configured)
            .finish_non_exhaustive()
    }
}

impl SoftPipeline {
    /// Opens `camera` for `settings` (a raw format the software ISP reads) with the algorithms
    /// of `tuning`; the frame rate is held at the configured interval.
    pub fn open(
        mut camera: NativeCamera,
        settings: &StreamSettings,
        tuning: &Tuning,
        threads: usize,
    ) -> Result<Self> {
        let configured = camera.configure(settings)?;
        let info: &CameraInfo = camera.info();
        let format = RawFormat::from_fourcc(
            styx_core::prelude::FourCc::from(configured.fourcc.0),
            configured.mode.width,
            configured.mode.height,
        )
        .ok_or_else(|| {
            PipelineError::Config(format!(
                "{} is not a raw format the software ISP reads",
                configured.fourcc
            ))
        })?;
        let fps = configured.interval.fps();
        let sensor = SensorInfo::from_description(
            &info.description,
            &configured.mode.mode,
            &configured.mode.format,
        )?
        .with_fps(fps, fps)?;
        let soft = SoftLoop::new(sensor, format.packing, tuning, threads)?;
        let controls = camera.controls();
        Ok(Self {
            camera,
            controls,
            stream: None,
            soft,
            configured,
        })
    }

    /// The configuration in effect.
    pub fn configured(&self) -> &Configured {
        &self.configured
    }

    /// The loop (controller, controls, recording).
    pub fn soft_loop(&mut self) -> &mut SoftLoop {
        &mut self.soft
    }

    /// The camera.
    pub fn camera(&self) -> &NativeCamera {
        &self.camera
    }

    /// The camera's controls.
    pub fn controls(&self) -> &CameraControls {
        &self.controls
    }

    /// Resets the algorithms, requests their start-up exposure for frame 0 and starts
    /// streaming.
    pub fn start(&mut self) -> Result<()> {
        let start = self.soft.start()?;
        if let Some(r) = start.sensor {
            apply_request(&self.controls, &r)?;
        }
        self.stream = Some(self.camera.start()?);
        Ok(())
    }

    /// The stream, while started.
    pub fn stream(&self) -> Option<&FrameStream> {
        self.stream.as_ref()
    }

    /// Waits for the next raw frame, processes it into `out` at `scale`, runs the algorithms
    /// and hands a new sensor request to the control schedule. `None` at the end of the stream.
    pub fn next(
        &mut self,
        timeout: Duration,
        scale: Scale,
        out: OutputBuffers<'_>,
    ) -> Result<Option<SoftFrame>> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| PipelineError::Device("not started".into()))?;
        let Some(raw) = stream.next_blocking(timeout)? else {
            return Ok(None);
        };
        let seq = u64::from(raw.sequence);
        let controls = raw
            .controls
            .or_else(|| self.controls.applied(seq))
            .ok_or_else(|| PipelineError::Device(format!("no control values for frame {seq}")))?;
        let sensor = sensor_values(seq, &controls);
        let output = self
            .soft
            .process(raw.data(), raw.stride as usize, &sensor, scale, out)?;
        let request_lands = match &output.step.sensor {
            Some(r) => Some(apply_request(&self.controls, r)?),
            None => None,
        };
        Ok(Some(SoftFrame {
            raw,
            sensor,
            output,
            request_lands,
        }))
    }

    /// Stops streaming.
    pub fn stop(&mut self) -> Result<()> {
        self.stream = None;
        self.camera.stop()?;
        Ok(())
    }

    /// Stops and powers the camera down.
    pub fn close(mut self) -> Result<()> {
        self.stream = None;
        self.camera.close()?;
        Ok(())
    }
}
