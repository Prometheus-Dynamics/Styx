//! The PiSP path for Styx's native cameras: neutral [`IspSettings`] mapped into the Lemnos
//! library's PiSP front and back end configuration, the tiling and the statistics layouts the
//! library owns, and the ISP jobs that carry them.
//!
//! * [`back_end`]: the back-end configuration for each frame (`BackEndBuilder`): black levels,
//!   white balance and digital gain, colour matrix, tone curve, lens shading, the denoise blocks
//!   (temporal denoise follows the exposure), the two outputs (full size, and output 1 as a 2x2
//!   box), and the split of a frame into two jobs so its first rows are out early.
//! * [`front_end`]: the front end's black levels and statistics weights.
//! * [`stats`]: the statistics buffer, parsed with the library's layouts into the controller's
//!   statistics.
//! * [`job`]: the `camera.isp/1` job and the bytes of the config buffer, and the sink jobs go to.
//!
//! Values are Styx's; layouts and tiling are the library's. Blocks the library does not carry
//! yet are refused rather than written here: see [`PispError::Unsupported`].
//!
//! Untested on board (H1): the bytes are checked against dev's configuration for recorded
//! settings (`tests/dev_golden.rs`), not against the PiSP itself.

pub mod back_end;
pub mod front_end;
pub mod job;
pub mod stats;

pub use back_end::{BackEndBuilder, Geometry};
pub use front_end::FrontEndBuilder;
pub use job::JobSink;
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
    /// A job or config buffer did not encode.
    Job(String),
    /// The job stream refused or failed the post.
    Sink(String),
    /// A statistics buffer did not parse.
    Stats(String),
}

impl std::fmt::Display for PispError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "pisp configuration: {e}"),
            Self::Unsupported(what) => write!(f, "pisp: not mapped yet: {what}"),
            Self::Job(e) => write!(f, "pisp job: {e}"),
            Self::Sink(e) => write!(f, "pisp job stream: {e}"),
            Self::Stats(e) => write!(f, "pisp statistics: {e}"),
        }
    }
}

impl std::error::Error for PispError {}
