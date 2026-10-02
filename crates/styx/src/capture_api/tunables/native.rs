//! The ISP outputs of a native camera's processed capture.

use styx_core::prelude::FourCc;

use super::StyxConfig;

/// What a native camera's ISP delivers for a processed (`NV12` / `RG24`) mode. The PiSP's
/// back end makes two outputs from one pass over each raw frame: the main one and a second
/// one (with the downscaler) attached to every frame as a `CompanionKind::Scaled` companion
/// with the same timestamp. Both are dma-bufs, handed out without a copy. The software ISP
/// ignores these settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct NativeIspConfig {
    /// Deliver frames at this size instead of the mode's (the ISP scales the whole field of
    /// view). `None` (default) uses the mode's size.
    pub output_size: Option<(u32, u32)>,
    /// Deliver frames in this processed format (`NV12` or `RG24`) instead of the mode's.
    pub output_format: Option<FourCc>,
    /// Also deliver each frame at this size and format from the second output, as a
    /// `CompanionKind::Scaled` companion. `None` (default) leaves it off.
    pub second_output: Option<((u32, u32), FourCc)>,
}

impl StyxConfig {
    /// Have a native camera's ISP deliver frames at `width`x`height` (see
    /// [`NativeIspConfig::output_size`]).
    pub fn native_output_size(mut self, width: u32, height: u32) -> Self {
        self.backends.native.output_size = Some((width, height));
        self
    }

    /// Have a native camera's ISP deliver frames in `format` (see
    /// [`NativeIspConfig::output_format`]).
    pub fn native_output_format(mut self, format: FourCc) -> Self {
        self.backends.native.output_format = Some(format);
        self
    }

    /// Also deliver each frame at `width`x`height` in `format` from a native camera's second
    /// ISP output (see [`NativeIspConfig::second_output`]).
    pub fn native_second_output(mut self, width: u32, height: u32, format: FourCc) -> Self {
        self.backends.native.second_output = Some(((width, height), format));
        self
    }
}
