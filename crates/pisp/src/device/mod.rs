//! Running the PiSP nodes through `styx-kernel` (feature `device`).
//!
//! - [`FrontEndDevice`]: the `rp1-cfe` media graph with the sensor → `csi2` → `pisp-fe`
//!   path, per-frame config buffers on `rp1-cfe-fe_config`, statistics from
//!   `rp1-cfe-fe_stats` and raw frames from `rp1-cfe-fe_image0`.
//! - [`BackEndDevice`]: one `pispbe` node group, memory to memory: `pispbe-input` →
//!   `pispbe-output0`, driven by `pispbe-config`.
//! - [`BackEndStream`]: a node group for a stream: dma-buf input (the front end's raw
//!   buffers), two outputs, a config per job, output buffers held until released.

mod be;
mod be_output;
mod be_spare;
mod be_stream;
mod be_tdn;
mod config_buf;
#[cfg(feature = "debug-hooks")]
mod config_dump;
#[cfg(not(feature = "debug-hooks"))]
mod config_dump {
    //! Debug hooks off (feature `debug-hooks`): the config dumps and the patch do nothing.
    use crate::uapi::BeTilesConfig;

    pub(super) fn be_config(_bytes: &[u8]) {}

    pub(super) fn fe_config(_bytes: &[u8]) {}

    pub(super) fn be_patch(_cfg: &BeTilesConfig) -> Option<Box<BeTilesConfig>> {
        None
    }
}
mod fe;
#[cfg(feature = "debug-hooks")]
mod lines_probe;
#[cfg(not(feature = "debug-hooks"))]
mod lines_probe {
    //! Debug hooks off (feature `debug-hooks`): no probe thread, no samples.
    use std::path::Path;

    pub(super) struct LinesProbe;

    impl LinesProbe {
        pub(super) fn start(
            _node: &Path,
            _bufs: Vec<(*const u8, usize)>,
            _stride: usize,
        ) -> Option<Self> {
            None
        }

        pub(super) fn check(&mut self, _seq: u32, _index: u32, _data: &[u8], _stride: usize) {}
    }
}
pub mod profile;
mod queue;

use std::fmt;
use std::path::PathBuf;

/// `STYX_CFE_EVENT_LINES - V4L2_EVENT_PRIVATE_START` (kernel-modules/rp1-cfe): the front end's
/// line event, subscribed to for slicing (and by the lines probe).
const LINES_EVENT_OFFSET: u32 = 0x5354;

pub use be::{BackEndDevice, BeOutput};
pub use be_output::OutputMemory;
pub use be_stream::{BackEndStream, BeFormat, BeJob, BeOutputSetup, QueuedJob};
#[cfg(feature = "debug-hooks")]
pub use config_buf::CONFIG_HEAP_ENV;
pub use config_buf::ConfigMemory;
pub use fe::{
    FeFrame, FeHeld, FeHeldRaw, FrontEndDevice, FrontEndSetup, HeldImage, LineEvent, SensorWindow,
};
pub use queue::Queue;

/// A device error.
#[derive(Debug)]
pub enum DeviceError {
    /// A kernel call failed.
    Kernel(styx_kernel::Error),
    /// The graph or a format is not what we need.
    Setup(String),
    /// A config was refused by the builder.
    Config(String),
    /// Nothing arrived in time.
    Timeout(&'static str),
    /// Every buffer of this output is held by the caller (frames consumers still hold): the
    /// job was not queued. Releasing one lets the next job run.
    OutputsHeld(usize),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kernel(e) => write!(f, "{e}"),
            Self::Setup(s) => write!(f, "setup: {s}"),
            Self::Config(s) => write!(f, "config: {s}"),
            Self::Timeout(what) => write!(f, "timed out waiting for {what}"),
            Self::OutputsHeld(i) => write!(f, "output {i}: every buffer is held"),
        }
    }
}

impl std::error::Error for DeviceError {}

impl From<styx_kernel::Error> for DeviceError {
    fn from(e: styx_kernel::Error) -> Self {
        Self::Kernel(e)
    }
}

/// Result alias.
pub type Result<T> = std::result::Result<T, DeviceError>;

/// Media devices whose driver is `driver` (e.g. `rp1-cfe`, `pispbe`), in path order.
pub fn find_media(driver: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = styx_kernel::media::list_media_devices()
        .into_iter()
        .filter(|p| {
            styx_kernel::media::MediaDevice::open_read_only(p)
                .and_then(|m| m.device_info())
                .is_ok_and(|i| i.driver == driver)
        })
        .collect();
    v.sort();
    v
}

/// `V4L2_PIX_FMT_SBGGR16` and friends: 16-bit Bayer fourcc for a PiSP Bayer order.
pub fn bayer16_fourcc(order: crate::uapi::BayerOrder) -> styx_kernel::FourCc {
    use crate::uapi::BayerOrder::*;
    styx_kernel::FourCc::new(match order {
        Rggb => b"RG16",
        Gbrg => b"GB16",
        Bggr => b"BYR2",
        Grbg => b"GR16",
        Greyscale => b"Y16 ",
    })
}
