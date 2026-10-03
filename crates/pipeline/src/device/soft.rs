//! The software ISP path on a native camera: packed raw frames from the receiver's raw node.

use std::time::Duration;

use styx_algo::{Tuning, WarmStart};
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
    warm_override: Option<Option<WarmStart>>,
}

/// Time from the end of a frame's readout until the software ISP's statistics have been
/// through the algorithms (one core at 1280x800), with slack.
const SOFT_PROCESSING: Duration = Duration::from_millis(16);

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
        let mut soft = SoftLoop::new(sensor, format.packing, tuning, threads)?;
        // Buffers from a (cached) dma-heap are read in place: 0.17 ms per 1280x800 frame
        // less than staging the rows on the CM5. The driver's MMAP buffers are often mapped
        // uncached (rp1-cfe's are) and keep the staging copy.
        let cached = matches!(
            &camera.options().memory,
            styx_native::BufferMemory::DmaHeap(heap) if !heap.contains("uncached")
        );
        soft.set_copy_input(!cached);
        let controls = camera.controls();
        Ok(Self {
            camera,
            controls,
            stream: None,
            soft,
            configured,
            warm_override: None,
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

    /// Process frames on `context`'s GPU (see [`SoftLoop::use_gpu`]): capture buffers that
    /// are dma-bufs are then read by the GPU in place, without a CPU mapping or cache sync.
    #[cfg(feature = "gpu")]
    pub fn use_gpu(&mut self, context: &styx_gpuisp::GpuContext) -> Result<()> {
        self.soft.use_gpu(context)
    }

    /// The camera.
    pub fn camera(&self) -> &NativeCamera {
        &self.camera
    }

    /// The camera's controls.
    pub fn controls(&self) -> &CameraControls {
        &self.controls
    }

    /// Starts the next [`Self::start`] (only) from these settled values (`None`: from the
    /// tuning's start-up values) instead of what the camera's last session settled on
    /// ([`crate::warm::recall`], the default).
    pub fn set_warm_start(&mut self, warm: Option<WarmStart>) {
        self.warm_override = Some(warm);
    }

    /// Resets the algorithms (from the camera's last settled state, see [`crate::warm`]),
    /// requests their start-up exposure for frame 0 (written before streaming) and starts
    /// streaming. Requests are written as soon as they are made while enough of the frame is
    /// left.
    pub fn start(&mut self) -> Result<()> {
        let latency = self
            .soft
            .info()
            .issue_latency(SOFT_PROCESSING, styx_native::control::DEFAULT_WRITE_MARGIN)
            .max(1);
        let warm = match self.warm_override.take() {
            Some(w) => w,
            None => crate::warm::recall(&self.camera.info().key),
        };
        let c = self.soft.controller();
        c.set_issue_latency(latency);
        c.set_warm_start(warm);
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
        let frame = match raw.dmabuf() {
            #[cfg(feature = "gpu")]
            Some(fd) if matches!(self.soft.engine(), crate::IspEngine::Gpu { .. }) => {
                crate::RawFrame::DmaBuf {
                    fd,
                    len: raw.buffer_len(),
                }
            }
            _ => crate::RawFrame::Bytes(raw.data()),
        };
        let output = self
            .soft
            .process_frame(frame, raw.stride as usize, &sensor, scale, out)?;
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

    /// Stops streaming; what the algorithms settled on is remembered for the camera's next
    /// start ([`crate::warm`]).
    pub fn stop(&mut self) -> Result<()> {
        self.remember();
        self.stream = None;
        self.camera.stop()?;
        Ok(())
    }

    fn remember(&mut self) {
        if self.stream.is_some()
            && let Some(w) = self.soft.controller().warm_state()
        {
            crate::warm::remember(&self.camera.info().key, w);
        }
    }

    /// Stops and powers the camera down.
    pub fn close(mut self) -> Result<()> {
        self.remember();
        self.stream = None;
        self.camera.close()?;
        Ok(())
    }
}
