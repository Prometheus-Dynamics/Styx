//! How the PiSP path is set up.

use styx_pisp::device::{BeFormat, BeOutputSetup, OutputMemory};
use styx_pisp::uapi::BeCropConfig;

/// How the PiSP path is set up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PispOptions {
    /// Back end outputs: format and size (output 1 has the downscaler).
    pub outputs: [Option<BeOutputSetup>; 2],
    /// Each output's crop of the sensor frame (`None`: all of it). An output without a size of
    /// its own is the crop at full resolution, written into the top left of its buffer (rows
    /// keep the buffer's stride; the chroma plane stays where the buffer's size puts it).
    /// Changeable while running ([`super::PispPipeline::set_output_crop`]).
    pub crop: [Option<BeCropConfig>; 2],
    /// Front end buffers per queue (raw frames, statistics, configs).
    pub fe_buffers: u32,
    /// Back end buffers per output.
    pub be_buffers: u32,
    /// Back end node group (0 or 1).
    pub be_group: usize,
    /// Front end configs queued ahead of the frames.
    pub configs_ahead: usize,
    /// While AE is locked and AWB has converged, run the algorithms at about this rate
    /// instead of on every frame (statistics are then not read on the other frames); any
    /// frame that finds them unsettled goes back to every frame. `None`: every frame.
    pub settled_rate_hz: Option<f64>,
    /// Where the back end's output buffers come from: a cached dma-heap (the default: CPU
    /// reads at memory speed, bracketed by [`super::PispPipeline::sync_output`]) or the driver's
    /// (mapped uncached).
    pub output_memory: OutputMemory,
    /// Run the back end's temporal denoise when the tuning has it (two extra buffers of the
    /// raw frame's size, read and written by every job).
    pub temporal_denoise: bool,
    /// Spatial and colour denoise thresholds times this (1: as tuned, 0: off; see
    /// [`crate::Controller::set_spatial_denoise`]).
    pub spatial_denoise: f64,
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
            crop: [None; 2],
            fe_buffers: 6,
            be_buffers: 6,
            be_group: 0,
            configs_ahead: 2,
            settled_rate_hz: Some(15.0),
            output_memory: OutputMemory::CachedHeap,
            temporal_denoise: true,
            spatial_denoise: 1.0,
        }
    }
}
