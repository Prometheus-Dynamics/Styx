//! The PiSP path for Styx's native cameras: neutral [`IspSettings`] mapped into the Lemnos
//! library's PiSP front and back end configuration, the tiling and the statistics layouts the
//! library owns, and the ISP jobs that carry them.
//!
//! * [`back_end`]: the back-end configuration for each frame (`BackEndBuilder`): black levels,
//!   white balance and digital gain, colour matrix, tone curve, lens shading, the denoise blocks
//!   (temporal denoise follows the exposure), sharpening and the other blocks at the library's
//!   defaults (`be::defaults`), the YCbCr conversion of each YUV output from its colour space and
//!   range, and the two outputs (full size, and output 1 as a 2x2 box).
//! * [`format`]: the FourCcs the back end writes and the scaling each output can do.
//! * [`front_end`]: the front end's black levels and statistics weights.
//! * [`stats`]: the statistics buffer, parsed with the library's layouts into the controller's
//!   statistics.
//! * [`job`]: the `camera.isp/1` job and the bytes of the config buffer, and the sink jobs go to.
//! * [`live`]: the live job path on the simulated and the reference board's ISP: setup, one job
//!   per frame, and the completions as leases (the sequence of the library's README).
//!
//! Values are Styx's; layouts and tiling are the library's. Blocks the library does not carry
//! yet are refused rather than written here: see [`PispError::Unsupported`].
//!
//! Untested on board (H1): the configuration bytes are checked against dev's for the recorded
//! settings (`tests/dev_golden.rs`, every block equal), and the live path against a simulated ISP
//! (`tests/live_sim.rs`), not against the PiSP itself.

pub mod back_end;
pub mod format;
pub mod front_end;
pub mod job;
pub mod live;
pub mod stats;

pub use back_end::{BackEndBuilder, Geometry, Output};
pub use front_end::FrontEndBuilder;
pub use job::JobSink;
pub use live::{LiveIsp, LiveJob};
pub use styx_settings::IspSettings;

/// The library's PiSP layouts and tiling, re-exported for callers that need them.
pub use styx_lemnos::pisp;

/// Why a frame's PiSP configuration could not be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PispError {
    /// The library refused the configuration (before anything is sent).
    Config(String),
    /// Settings the library carries no block for yet: a gap, named here, not written by Styx.
    Unsupported(&'static str),
    /// A job was not posted, and nothing it names was changed: the caller tries again once a job
    /// in flight completes (two config buffers, the output buffers or the job ring are in use).
    Busy(&'static str),
    /// The camera is not set up as the live path needs (a stream is missing, a size is wrong).
    Setup(String),
    /// A job or config buffer did not encode.
    Job(String),
    /// The job stream refused or failed the post.
    Sink(String),
    /// A statistics buffer did not parse.
    Stats(String),
    /// Jobs were in flight and did not complete within the wait's timeout: the flights, and the
    /// device's counters at that point.
    Stalled(String),
}

impl std::fmt::Display for PispError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "pisp configuration: {e}"),
            Self::Unsupported(what) => write!(f, "pisp: not mapped yet: {what}"),
            Self::Busy(why) => write!(f, "pisp job not posted: {why}"),
            Self::Setup(e) => write!(f, "pisp setup: {e}"),
            Self::Job(e) => write!(f, "pisp job: {e}"),
            Self::Sink(e) => write!(f, "pisp job stream: {e}"),
            Self::Stats(e) => write!(f, "pisp statistics: {e}"),
            Self::Stalled(e) => write!(f, "pisp jobs stalled: {e}"),
        }
    }
}

impl std::error::Error for PispError {}
