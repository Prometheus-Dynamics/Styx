//! Runtime codec limits.

use styx_codec::prelude::{DEFAULT_CODEC_MAX_HEIGHT, DEFAULT_CODEC_MAX_WIDTH};

/// Runtime codec registration limits.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct CodecConfig {
    /// Maximum encoded/decoded frame width advertised by built-in codecs.
    pub max_width: u32,
    /// Maximum encoded/decoded frame height advertised by built-in codecs.
    pub max_height: u32,
}

impl Default for CodecConfig {
    fn default() -> Self {
        Self {
            max_width: DEFAULT_CODEC_MAX_WIDTH,
            max_height: DEFAULT_CODEC_MAX_HEIGHT,
        }
    }
}

impl CodecConfig {
    pub(crate) fn sanitized(self) -> Self {
        Self {
            max_width: self.max_width.max(1),
            max_height: self.max_height.max(1),
        }
    }
}
