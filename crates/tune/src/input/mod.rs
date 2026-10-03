//! Reading raw captures: Styx raw recordings (`.jsonl` + `.raw`, `styx-pipeline::rawrec`), Styx
//! MCAP recordings (feature `mcap`: frames recorded by `StreamRecorder` with each frame's
//! exposure and gains) and DNG files.
//!
//! DNG decoding sits behind [`RawDecoder`]: the built-in [`dng::MinimalDng`] reads the
//! uncompressed CFA DNGs cameras and `picamera2` write; a fuller decoder can be plugged in with
//! [`Loader::with_dng`].

pub mod dng;
#[cfg(feature = "mcap")]
pub mod mcap;
pub mod rawrec;

use std::path::Path;

use crate::error::{Result, TuneError};
use crate::raw::RawFrame;

/// Decodes a file's bytes into raw frames.
pub trait RawDecoder: Send + Sync {
    /// The frames in `bytes` (one for a DNG).
    fn decode(&self, bytes: &[u8]) -> Result<Vec<RawFrame>>;
}

/// Reads capture files by extension.
pub struct Loader {
    dng: Box<dyn RawDecoder>,
}

impl Default for Loader {
    fn default() -> Self {
        Self {
            dng: Box::new(dng::MinimalDng),
        }
    }
}

impl Loader {
    /// The built-in readers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Use another DNG decoder.
    pub fn with_dng(mut self, decoder: Box<dyn RawDecoder>) -> Self {
        self.dng = decoder;
        self
    }

    /// Every frame in `path`: `.dng`/`.tif(f)` as DNG, `.mcap` as a Styx recording, `.jsonl` or
    /// `.raw` (or a base name with both next to it) as a Styx raw recording.
    pub fn load(&self, path: &Path) -> Result<Vec<RawFrame>> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        match ext.as_str() {
            "dng" | "tif" | "tiff" => {
                let bytes = std::fs::read(path).map_err(|e| TuneError::io(path, e))?;
                self.dng.decode(&bytes)
            }
            "mcap" => load_mcap(path),
            _ => rawrec::load(path),
        }
    }
}

#[cfg(feature = "mcap")]
fn load_mcap(path: &Path) -> Result<Vec<RawFrame>> {
    mcap::load(path)
}

#[cfg(not(feature = "mcap"))]
fn load_mcap(path: &Path) -> Result<Vec<RawFrame>> {
    Err(TuneError::Format(format!(
        "{}: MCAP recordings need styx-tune's `mcap` feature",
        path.display()
    )))
}
