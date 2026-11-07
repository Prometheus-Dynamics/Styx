//! How the PiSP path is set up.

use lemnos_drivers_pisp::device::{BeFormat, BeOutputSetup, ConfigMemory, OutputMemory};
use lemnos_drivers_pisp::uapi::BeCropConfig;

/// Debug hook (feature `pisp-debug-hooks`): the environment variable `name`, parsed. Without
/// the feature it is ignored, so the options are the only way to set these.
#[cfg(feature = "pisp-debug-hooks")]
pub(super) fn debug_var<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok()?.parse().ok()
}

/// See the feature-enabled version: the environment is not read.
#[cfg(not(feature = "pisp-debug-hooks"))]
pub(super) fn debug_var<T>(_name: &str) -> Option<T> {
    None
}

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
    /// Front end buffers per queue (raw frames, statistics, configs). Default 4: on Raze #1
    /// (OV9782 1280x800, 120 fps, 2026-11-06) 3 already delivered every processed frame, with
    /// or without the raw lease, and 4 kept a raw consumer holding 3 frames on four busy cores
    /// at the rate 6 did (a raw lease adds [`Self::RAW_LEASE_BUFFERS`] to this).
    pub fe_buffers: u32,
    /// Back end buffers per output.
    pub be_buffers: u32,
    /// Of [`Self::be_buffers`], how many cached heap buffers are allocated when the stream
    /// opens; the rest by a background allocator as frames need them, one spare ahead
    /// (`OutputMemory::CachedHeap` only). `None`: all, at open.
    pub be_buffers_initial: Option<u32>,
    /// More buffers per output, for the extra passes that write into them
    /// ([`super::PispPipeline::set_pass`]).
    pub pass_buffers: [u32; 2],
    /// Extra passes' jobs queued at once, up to [`lemnos_drivers_pisp::device::QUEUED_JOBS`]: `1` and `0`
    /// (default) wait for each before queuing the next. More than 1 is refused without
    /// [`Self::unsafe_queued_passes`]: the driver overwrites a job the hardware still holds
    /// ("not safe to queue new job!"), and on the CM5 (2026-10-31) a lost completion left
    /// `STREAMOFF` stuck in `vb2_wait_for_all_buffers` until a reboot.
    pub pass_jobs: usize,
    /// Allow [`Self::pass_jobs`] above 1 (it can hang; may need a reboot).
    pub unsafe_queued_passes: bool,
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
    /// Where the back end's config buffer comes from: a cached dma-heap (the default) or the
    /// driver's (mapped uncached). The driver copies each job's config out of it.
    pub config_memory: ConfigMemory,
    /// Run the back end's temporal denoise when the tuning has it (two extra buffers of the
    /// raw frame's size, read and written by every job).
    pub temporal_denoise: bool,
    /// Keep the temporal-denoise average compressed (8-bit PiSP mode 1, as libcamera does: half
    /// the TDN memory traffic) rather than 16-bit.
    pub tdn_compressed: bool,
    /// Spatial and colour denoise thresholds times this (1: as tuned, 0: off; see
    /// [`crate::Controller::set_spatial_denoise`]).
    pub spatial_denoise: f64,
    /// What extra passes ([`super::PispPipeline::set_pass`]) do with temporal denoise.
    pub pass_tdn: crate::pisp_passes::PassTdn,
    /// Process each frame in two back end jobs, the first once the raw frame's first `line`
    /// rows are in, the rest at its end: the frame is ready sooner after its readout. `None`
    /// (default): one job at the end. A frame whose rows are not in when checked runs whole;
    /// so does the stream on a receiver without `FRAME_SYNC`.
    pub slice_line: Option<u16>,
    /// Keep each frame's raw front end buffer dequeued after the back end read it, for a
    /// consumer of the raw frame (`PispFrame::raw_image`, [`super::PispPipeline::release_raw`]);
    /// give the front end extra `fe_buffers` for those held. `false` (default): requeued at once.
    pub raw_lease: bool,
    /// Raw frames between the front and back end in 8-bit PiSP compressed form
    /// ([`crate::pisp_raw_comp`]): half the raw buffers' memory. Not with `raw_lease` (its
    /// consumers take 16-bit samples). Default `false`; written, untested on board.
    pub raw_compressed: bool,
    /// How output 1 makes an image of exactly half its input (`HalfFilter::Box`, the default: a
    /// 2x2 box, as `box_downscale_luma_in`; see [`crate::pisp_half`]).
    pub half_filter: crate::pisp_half::HalfFilter,
}

impl PispOptions {
    /// [`Self::pass_jobs`] with `0` resolved.
    pub fn pass_jobs_resolved(&self) -> usize {
        self.pass_jobs.max(1)
    }

    /// Whether the raw frames are compressed: `raw_compressed` (or `STYX_PISP_RAW_COMPRESSED=1`)
    /// and no `raw_lease`, whose consumers take 16-bit samples.
    pub fn compressed_raw(&self) -> bool {
        let on =
            debug_var::<u8>("STYX_PISP_RAW_COMPRESSED").map_or(self.raw_compressed, |v| v != 0);
        on && !self.raw_lease
    }

    /// NV12 at the sensor size on output 0 and RGB24 at half size on output 1.
    pub fn nv12_and_half_rgb(width: u32, height: u32) -> Self {
        Self {
            outputs: [
                Some(BeOutputSetup {
                    format: BeFormat::Nv12,
                    width,
                    height,
                    gpu_pad: 256,
                }),
                Some(BeOutputSetup {
                    format: BeFormat::Rgb24,
                    width: width / 2,
                    height: height / 2,
                    gpu_pad: 256,
                }),
            ],
            crop: [None; 2],
            fe_buffers: 4,
            be_buffers: 6,
            be_buffers_initial: None,
            pass_buffers: [0; 2],
            pass_jobs: 0,
            unsafe_queued_passes: false,
            be_group: 0,
            configs_ahead: 2,
            settled_rate_hz: Some(15.0),
            output_memory: OutputMemory::CachedHeap,
            config_memory: ConfigMemory::CachedHeap,
            temporal_denoise: true,
            tdn_compressed: true,
            spatial_denoise: 1.0,
            pass_tdn: Default::default(),
            slice_line: None,
            raw_lease: false,
            raw_compressed: false,
            half_filter: Default::default(),
        }
    }
}
