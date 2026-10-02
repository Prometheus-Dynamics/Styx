//! The ISP outputs of a native camera's processed capture.

use styx_core::prelude::FourCc;

use super::StyxConfig;

/// What a native camera's ISP delivers for a processed (`NV12` / `RG24`) mode. The PiSP's
/// back end makes two outputs from one pass over each raw frame: the main one and a second
/// one (with the downscaler) attached to every frame as a `CompanionKind::Scaled` companion
/// with the same timestamp. Both are dma-bufs, handed out without a copy. The software ISP
/// uses only [`Self::driver_buffers`] and [`Self::soft_threads`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// Attach the second output as a pyramid companion at `2^-pyramid_level` of the main
    /// output's size, in its format (1 = ½, 2 = ¼, 3 = ⅛; 0, the default, off), read with
    /// `FrameLease::pyramid_level`. Takes the second output: `second_output` is then ignored.
    pub pyramid_level: u8,
    /// Use the ISP driver's own buffers, which the CPU reads uncached (the Y plane of a
    /// 1280x800 frame in 1.9 ms on the CM5). `false` (default): cached dma-heap buffers, read
    /// at memory speed (cache maintenance only when a frame's pixels are read). With the
    /// software ISP this is the raw capture's buffers (the receiver's MMAP buffers cost the
    /// software ISP 0.6 ms more per 1280x800 frame on the CM5).
    pub driver_buffers: bool,
    /// Threads of the software ISP (cameras without a PiSP). `None` (default): one per core,
    /// at most 4.
    pub soft_threads: Option<usize>,
    /// Temporal denoise in the PiSP's back end when the tuning has it (`true`, the default):
    /// a running average of the frame, two extra raw-sized buffers read and written by every
    /// job; it brings flat-area noise down to libcamera's (4x lower than spatial denoise
    /// alone on the OV9782) at about 1.5 ms more back end time per 1280x800 frame (latency,
    /// not CPU). `false`: spatial and colour denoise only, at their no-TDN strengths.
    pub temporal_denoise: bool,
    /// Strength of the PiSP's spatial and colour denoise in percent of the tuning's: their
    /// noise thresholds are scaled by this (100, the default: as tuned; 0: off).
    pub spatial_denoise_percent: u16,
}

impl Default for NativeIspConfig {
    fn default() -> Self {
        Self {
            output_size: None,
            output_format: None,
            second_output: None,
            pyramid_level: 0,
            driver_buffers: false,
            soft_threads: None,
            temporal_denoise: true,
            spatial_denoise_percent: 100,
        }
    }
}

impl StyxConfig {
    /// Run a native camera's software ISP on `threads` threads (see
    /// [`NativeIspConfig::soft_threads`]).
    pub fn native_soft_threads(mut self, threads: usize) -> Self {
        self.backends.native.soft_threads = Some(threads.max(1));
        self
    }

    /// Turn a native camera's temporal denoise on or off (see
    /// [`NativeIspConfig::temporal_denoise`]).
    pub fn native_temporal_denoise(mut self, on: bool) -> Self {
        self.backends.native.temporal_denoise = on;
        self
    }

    /// Scale a native camera's spatial and colour denoise to `percent` of the tuning's (see
    /// [`NativeIspConfig::spatial_denoise_percent`]).
    pub fn native_spatial_denoise(mut self, percent: u16) -> Self {
        self.backends.native.spatial_denoise_percent = percent;
        self
    }

    /// Have a native camera's ISP write into its driver's buffers (see
    /// [`NativeIspConfig::driver_buffers`]).
    pub fn native_driver_buffers(mut self, driver: bool) -> Self {
        self.backends.native.driver_buffers = driver;
        self
    }

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

    /// Attach a pyramid companion from a native camera's second ISP output (see
    /// [`NativeIspConfig::pyramid_level`]).
    pub fn native_pyramid_level(mut self, level: u8) -> Self {
        self.backends.native.pyramid_level = level.min(3);
        self
    }

    /// Also deliver each frame at `width`x`height` in `format` from a native camera's second
    /// ISP output (see [`NativeIspConfig::second_output`]).
    pub fn native_second_output(mut self, width: u32, height: u32, format: FourCc) -> Self {
        self.backends.native.second_output = Some(((width, height), format));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denoise_defaults_on_and_builders_set_it() {
        let d = NativeIspConfig::default();
        assert!(d.temporal_denoise);
        assert_eq!(d.spatial_denoise_percent, 100);
        let c = StyxConfig::default()
            .native_temporal_denoise(false)
            .native_spatial_denoise(50);
        assert!(!c.backends.native.temporal_denoise);
        assert_eq!(c.backends.native.spatial_denoise_percent, 50);
    }
}
