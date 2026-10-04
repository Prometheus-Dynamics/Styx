//! What [`super::PispPipeline`] hands out per frame and at start-up.

use std::time::{Duration, Instant};

use styx_pisp::device::BeJob;

use crate::controller::SensorValues;

/// Where one frame's time went.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PispTimes {
    /// Converting the statistics for the algorithms.
    pub stats: Duration,
    /// Running the algorithms on this frame's statistics (while the back end works).
    pub algorithms: Duration,
    /// Updating the back end config (and its tiles, when they change).
    pub be_prepare: Duration,
    /// The back end job (config queued to output dequeued).
    pub be_job: Duration,
    /// From the front end's buffers being dequeued to the outputs being ready (extra passes
    /// included).
    pub total: Duration,
    /// The extra passes ([`super::PispPipeline::set_pass`]): their configs and jobs, from
    /// the main job done to the last pass done.
    pub passes: Duration,
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
    /// The back end job: hand it to [`PispPipeline::output`] and [`PispPipeline::release`].
    pub job: BeJob,
    /// The statistics buffer's sequence differed from the raw frame's.
    pub sequence_mismatch: bool,
    /// Frame the sensor request made from this frame's statistics lands on, if one was made.
    pub request_lands: Option<u64>,
    /// The frame whose statistics the back end settings came from (`None` before the
    /// algorithms have seen a frame).
    pub settings_from: Option<u64>,
    /// The digital gain the back end gave this frame (with the white balance's green gain).
    pub digital_gain: f64,
    /// The flicker brightness deflicker took out of this frame (1: none).
    pub flicker: f64,
    /// Time spent.
    pub times: PispTimes,
    /// The raw frame, copied when [`super::PispPipeline::set_raw_copy`] asked for it.
    pub raw: Option<Box<crate::still::HeldRaw>>,
    /// The extra passes over this frame's raw input, by slot ([`super::PispPipeline::set_pass`];
    /// `None`: no pass in the slot, or no free buffer for it this frame). Their buffers go back
    /// with [`super::PispPipeline::release_output`].
    pub passes: Vec<Option<PassOutput>>,
}

/// One extra pass over a frame: the output buffer it wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PassOutput {
    /// The output whose buffer holds it.
    pub output: usize,
    /// The buffer.
    pub index: u32,
    /// The job (config queued to output done).
    pub elapsed: Duration,
    /// The pass as it was made: the region of the frame, its output size and format.
    pub spec: crate::pisp_passes::PassSpec,
}
