//! The ISP outputs of a native camera's processed capture.

use styx_core::prelude::{FourCc, FrameRect};

use super::StyxConfig;

/// Flicker avoidance of a native camera's processed modes (their AE): exposures of whole
/// flicker periods when they are a period or longer, and AE metering against the mean light
/// so shorter ones (8 ms at 120 fps) do not chase the beat flickering light puts on the frames.
/// Also the `AE_FLICKER_MODE` control (`native_controls`), as [`Self::control_value`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum NativeFlicker {
    /// No avoidance.
    Off,
    /// 50 Hz mains (light flickering at 100 Hz).
    Mains50,
    /// 60 Hz mains (light flickering at 120 Hz).
    Mains60,
    /// Detect 50 or 60 Hz flicker from the frames and avoid it once found (the default).
    #[default]
    Auto,
}

impl NativeFlicker {
    /// The `AE_FLICKER_MODE` control value: 0 off, 1 50 Hz, 2 60 Hz, 3 auto.
    pub fn control_value(self) -> i32 {
        match self {
            Self::Off => 0,
            Self::Mains50 => 1,
            Self::Mains60 => 2,
            Self::Auto => 3,
        }
    }

    /// From an `AE_FLICKER_MODE` control value.
    pub fn from_control_value(v: i64) -> Option<Self> {
        Some(match v {
            0 => Self::Off,
            1 => Self::Mains50,
            2 => Self::Mains60,
            3 => Self::Auto,
            _ => return None,
        })
    }
}

/// Taking light flicker out of a native camera's processed frames (exposures shorter than
/// a flicker period, e.g. 8 ms at 120 fps, keep the flicker the frames see even when AE no
/// longer chases it): each frame's ISP digital gain divided by the brightness AE's flicker
/// model predicts for it. Also the `AE_DEFLICKER_MODE` control, as [`Self::control_value`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum NativeDeflicker {
    /// Never.
    Off,
    /// Whenever the frames flicker, also with flicker avoidance off.
    On,
    /// Whenever flicker avoidance ([`NativeFlicker`]) is on (the default).
    #[default]
    Auto,
}

impl NativeDeflicker {
    /// The `AE_DEFLICKER_MODE` control value: 0 off, 1 on, 2 auto.
    pub fn control_value(self) -> i32 {
        match self {
            Self::Off => 0,
            Self::On => 1,
            Self::Auto => 2,
        }
    }

    /// From an `AE_DEFLICKER_MODE` control value.
    pub fn from_control_value(v: i64) -> Option<Self> {
        Some(match v {
            0 => Self::Off,
            1 => Self::On,
            2 => Self::Auto,
            _ => return None,
        })
    }
}

/// Regions of interest a native camera's PiSP makes besides its main output (see
/// [`NativeIspConfig::regions`]).
pub const MAX_NATIVE_REGIONS: usize = 15;

/// A region of interest the PiSP crops at full resolution besides the main output, delivered
/// with every frame as a `CompanionKind::Region` companion (see [`NativeIspConfig::regions`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct NativeRegion {
    /// Where it starts (frame pixels, rounded out to even pixels, at least 16x16). `None`: no
    /// region (no companion) until its control sets one.
    pub rect: Option<FrameRect>,
    /// Its format, `NV12` or `RG24` (`None`: the main output's). Only the region the second
    /// output makes can differ from the main output's.
    pub format: Option<FourCc>,
}

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
    /// PiSP, with the main output at the mode's size: deliver this region of the frame at
    /// full resolution from the start (rounded out to even pixels, at least 16x16). The
    /// `OUTPUT_CROP` control changes it while running; frames carry it as `FrameMeta::crop`.
    /// `None` (default): the whole frame.
    pub crop: Option<FrameRect>,
    /// PiSP: attach the whole frame scaled to this size, in the main output's format, to every
    /// frame as a `CompanionKind::Overview` companion (`FrameLease::overview`), from the second
    /// output; with [`Self::crop`], a low-resolution view of everything around the region.
    /// Takes the second output: `pyramid_level` and `second_output` are then ignored. `None`
    /// (default): off. With [`Self::pyramid_level`] too, the pyramid level comes from an extra
    /// back end pass (of the main output's region, following it), as with [`Self::crop`].
    pub overview: Option<(u32, u32)>,
    /// PiSP, with the main output at the mode's size: more regions of the frame, each cropped
    /// at full resolution from the same raw frame and attached to every frame as a
    /// `CompanionKind::Region { index }` companion (`FrameLease::region`; region `k` of this
    /// list is index `k + 1`, the main output index 0), moved while running by the
    /// `region_crop(index)` controls. The first comes from the second output when nothing else
    /// takes it and the main output is the whole frame (whose pass then covers every region at
    /// no extra cost); the others from extra back end passes over the raw frame, one per
    /// region (about 0.05 ms of back end time for a 128x128 region, 0.4 ms for 640x400, and
    /// 0.02 ms of CPU each on the CM5), in the main output's format. Empty slots make nothing.
    pub regions: [Option<NativeRegion>; MAX_NATIVE_REGIONS],
    /// Use the ISP driver's own buffers, which the CPU reads uncached (the Y plane of a
    /// 1280x800 frame in 1.9 ms on the CM5). `false` (default): cached dma-heap buffers, read
    /// at memory speed (cache maintenance only when a frame's pixels are read). With the
    /// software ISP this is the raw capture's buffers (the receiver's MMAP buffers cost the
    /// software ISP 0.6 ms more per 1280x800 frame on the CM5).
    pub driver_buffers: bool,
    /// Threads of the software ISP (cameras without a PiSP). `None` (default): half the
    /// cores, at most 4 (2 on the CM5). More can hang a CM5 at 2.4 GHz within seconds (three
    /// at 120 fps, four at any rate: `docs/native-stack/pipeline.md`, "All four cores").
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
    /// Buffers of each PiSP back end output (default 6, at least 2). Frames consumers hold
    /// (a frame server's latest frame, frames other processes have not released) keep theirs;
    /// when every one is held, frames are dropped until one comes back. More buffers let slow
    /// consumers hold frames without costing frames, at one output frame's memory each
    /// (1.5 MB for NV12 1280x800). A capture queue or extra buffers beyond the defaults (the
    /// planner reserves every consumer's queue of a shared capture this way) add theirs.
    pub output_buffers: u32,
    /// Flicker avoidance of the 3A loop (default [`NativeFlicker::Auto`]).
    pub flicker: NativeFlicker,
    /// Deflicker of the 3A loop (default [`NativeDeflicker::Auto`]: on with flicker
    /// avoidance).
    pub deflicker: NativeDeflicker,
}

impl Default for NativeIspConfig {
    fn default() -> Self {
        Self {
            output_size: None,
            output_format: None,
            second_output: None,
            pyramid_level: 0,
            crop: None,
            overview: None,
            regions: [None; MAX_NATIVE_REGIONS],
            driver_buffers: false,
            soft_threads: None,
            temporal_denoise: true,
            spatial_denoise_percent: 100,
            output_buffers: 6,
            flicker: NativeFlicker::Auto,
            deflicker: NativeDeflicker::Auto,
        }
    }
}

impl NativeIspConfig {
    /// The region (slot in [`Self::regions`]) the second output makes: the first one, when no
    /// overview, pyramid level or second output takes the second output and the main output is
    /// not cropped (its pass then reads the whole frame, so a crop on the second output costs
    /// nothing; two crops far apart in one pass cost more than a pass each, as all the tiles
    /// between them are processed).
    pub fn second_output_region(&self) -> Option<usize> {
        let taken = self.overview.is_some()
            || self.pyramid_level > 0
            || self.second_output.is_some()
            || self.crop.is_some();
        if taken {
            return None;
        }
        self.regions.iter().position(Option::is_some)
    }

    /// The regions extra back end passes make (slots in [`Self::regions`]).
    pub fn pass_regions(&self) -> impl Iterator<Item = usize> + '_ {
        let second = self.second_output_region();
        (0..MAX_NATIVE_REGIONS).filter(move |&k| self.regions[k].is_some() && Some(k) != second)
    }

    /// An extra back end pass makes the pyramid level: the second output makes the overview, or
    /// the main output is a crop (the second output would scale the whole frame, not the crop).
    pub fn pyramid_pass(&self) -> bool {
        self.pyramid_level > 0 && (self.overview.is_some() || self.crop.is_some())
    }
}

impl StyxConfig {
    /// Have a native camera's PiSP also crop these regions from every frame (see
    /// [`NativeIspConfig::regions`]; at most [`MAX_NATIVE_REGIONS`], the rest are ignored).
    pub fn native_regions(mut self, regions: &[NativeRegion]) -> Self {
        let mut slots = [None; MAX_NATIVE_REGIONS];
        for (slot, region) in slots.iter_mut().zip(regions) {
            *slot = Some(*region);
        }
        self.backends.native.regions = slots;
        self
    }

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

    /// Give each PiSP back end output `buffers` buffers (see
    /// [`NativeIspConfig::output_buffers`]).
    pub fn native_output_buffers(mut self, buffers: u32) -> Self {
        self.backends.native.output_buffers = buffers.max(2);
        self
    }

    /// Set a native camera's flicker avoidance (see [`NativeIspConfig::flicker`]).
    pub fn native_flicker(mut self, flicker: NativeFlicker) -> Self {
        self.backends.native.flicker = flicker;
        self
    }

    /// Set a native camera's deflicker (see [`NativeIspConfig::deflicker`]).
    pub fn native_deflicker(mut self, deflicker: NativeDeflicker) -> Self {
        self.backends.native.deflicker = deflicker;
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

    /// Deliver only `region` of the frame, at full resolution, from a native camera's ISP (see
    /// [`NativeIspConfig::crop`]).
    pub fn native_crop(mut self, region: FrameRect) -> Self {
        self.backends.native.crop = Some(region);
        self
    }

    /// Attach the whole frame at `width`x`height` to every frame from a native camera's second
    /// ISP output (see [`NativeIspConfig::overview`]).
    pub fn native_overview(mut self, width: u32, height: u32) -> Self {
        self.backends.native.overview = Some((width, height));
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
    fn the_second_output_makes_a_region_only_when_free_and_the_main_output_is_whole() {
        let r = NativeRegion {
            rect: Some(FrameRect::new(0, 0, 64, 64)),
            format: None,
        };
        let c = StyxConfig::default().native_regions(&[r, r, r]);
        let n = c.backends.native;
        assert_eq!(n.second_output_region(), Some(0));
        assert_eq!(n.pass_regions().collect::<Vec<_>>(), [1, 2]);
        let cropped = c.clone().native_crop(FrameRect::new(0, 0, 256, 256));
        assert_eq!(cropped.backends.native.second_output_region(), None);
        assert_eq!(cropped.backends.native.pass_regions().count(), 3);
        let overview = c.native_overview(320, 200).native_pyramid_level(1);
        assert_eq!(overview.backends.native.second_output_region(), None);
        assert!(overview.backends.native.pyramid_pass());
    }

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

    #[test]
    fn flicker_defaults_to_auto_and_maps_to_control_values() {
        assert_eq!(NativeIspConfig::default().flicker, NativeFlicker::Auto);
        assert_eq!(NativeIspConfig::default().output_buffers, 6);
        for f in [
            NativeFlicker::Off,
            NativeFlicker::Mains50,
            NativeFlicker::Mains60,
            NativeFlicker::Auto,
        ] {
            assert_eq!(
                NativeFlicker::from_control_value(i64::from(f.control_value())),
                Some(f)
            );
        }
        assert_eq!(NativeFlicker::from_control_value(4), None);
        let c = StyxConfig::default().native_flicker(NativeFlicker::Mains50);
        assert_eq!(c.backends.native.flicker, NativeFlicker::Mains50);
    }

    #[test]
    fn deflicker_defaults_to_auto_and_maps_to_control_values() {
        assert_eq!(NativeIspConfig::default().deflicker, NativeDeflicker::Auto);
        for d in [
            NativeDeflicker::Off,
            NativeDeflicker::On,
            NativeDeflicker::Auto,
        ] {
            assert_eq!(
                NativeDeflicker::from_control_value(i64::from(d.control_value())),
                Some(d)
            );
        }
        assert_eq!(NativeDeflicker::from_control_value(3), None);
        let c = StyxConfig::default().native_deflicker(NativeDeflicker::Off);
        assert_eq!(c.backends.native.deflicker, NativeDeflicker::Off);
    }
}
