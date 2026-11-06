//! Output 1 at exactly half its input as a 2x2 box average, the CPU's `box_downscale_luma_in`
//! filter, rather than the smart resize's corner-aligned Lanczos 3, which sharpens.
//!
//! Raze #1 (OV9782 1280x800 -> 640x400 luma, 150 live frames each, 2026-11-06), the PiSP level
//! against `box_downscale_luma_in` of the same frame's full luma:
//!
//! | filter | identical | \|diff\| <= 1 | max \|diff\| | mean (PiSP - CPU) | dequeue to ISP done, p50 |
//! |---|---|---|---|---|---|
//! | [`HalfFilter::Lanczos3`] | 66.0 % | 89.0 % | 38 | -0.14 | 1.769 ms |
//! | [`HalfFilter::BoxDownscaler`] | 87.7 % | 100.0 % | 3 | -0.11 | 1.771 ms |
//! | [`HalfFilter::Box`] | 89.8 % | 100.0 % | 2 (1 pixel) | -0.08 | 1.769 ms |
//!
//! The rest cannot match: the back end averages its luma before rounding it to 8 bits, the CPU
//! averages the rounded output (a Y offset on output 1's CSC only moves the difference: +1/8
//! LSB gives 90.2 % identical, +1/4 82.7 %). The back end time is the same for all three.

use lemnos_drivers_pisp::be::BackEnd;
use lemnos_drivers_pisp::uapi::{BeDownscaleExtra, BeResampleExtra, rgb_enable};

/// How output 1 makes an image of exactly half its input (other sizes use the smart resize).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HalfFilter {
    /// A 2x2 box (the default): the resampler at a step of exactly 2 with two equal taps, so
    /// output pixel `x` is the mean of input pixels `2x` and `2x + 1` on each axis.
    #[default]
    Box,
    /// A 2x2 box from the downscaler alone (an area average at 2x), the resampler off.
    BoxDownscaler,
    /// The smart resize: Lanczos 3, corner aligned (libcamera's).
    Lanczos3,
}

impl core::str::FromStr for HalfFilter {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "lanczos3" => Ok(Self::Lanczos3),
            "ds" | "box-downscaler" => Ok(Self::BoxDownscaler),
            "box" | "rs" => Ok(Self::Box),
            _ => Err(()),
        }
    }
}

/// Taps per phase of the resampler, and the tap at the output pixel's input position.
const TAPS: usize = 6;
const CENTRE: usize = 2;
/// 1.0 in the resampler's s.10 coefficients.
const ONE: i16 = 1 << 10;

/// Sets output 1 to `filter` when its size (its output format's) is exactly half of `input`
/// (output 1's crop, or the whole input); returns whether it did. Otherwise output 1 goes back
/// to the smart resize, so this can be called again whenever the crop changes.
pub fn set_half_filter(be: &mut BackEnd, filter: HalfFilter, input: (u16, u16)) -> bool {
    let image = be.config().output_format[1].image;
    let (w, h) = (image.width, image.height);
    be.set_resample_step(1, None);
    if filter == HalfFilter::Lanczos3
        || w == 0
        || (u32::from(w) * 2, u32::from(h) * 2) != (u32::from(input.0), u32::from(input.1))
    {
        be.set_smart_resize(1, w, h);
        return false;
    }
    be.set_smart_resize(1, 0, 0);
    let mut rgb =
        be.config().global.rgb_enables & !(rgb_enable::downscale(1) | rgb_enable::resample(1));
    match filter {
        HalfFilter::BoxDownscaler => {
            be.set_downscale(
                1,
                BeDownscaleExtra {
                    scaled_width: w,
                    scaled_height: h,
                },
            );
            rgb |= rgb_enable::downscale(1);
        }
        HalfFilter::Box => {
            be.set_resample(
                1,
                BeResampleExtra {
                    scaled_width: w,
                    scaled_height: h,
                    ..Default::default()
                },
            );
            be.set_resample_step(1, Some((2 << 12, 2 << 12)));
            let mut coef = [0i16; 96];
            for phase in coef.as_chunks_mut::<TAPS>().0 {
                phase[CENTRE] = ONE / 2;
                phase[CENTRE + 1] = ONE / 2;
            }
            be.config_mut().resample[1].coef = coef;
            rgb |= rgb_enable::resample(1);
        }
        HalfFilter::Lanczos3 => unreachable!(),
    }
    be.config_mut().global.rgb_enables = rgb;
    true
}

#[cfg(test)]
mod tests {
    use lemnos_drivers_pisp::format::{compute_stride_align, formats};
    use lemnos_drivers_pisp::uapi::{BayerOrder, ImageFormatConfig};

    use super::*;
    use crate::isp::be_template;

    fn fmt(w: u16, h: u16, format: u32) -> ImageFormatConfig {
        let mut f = ImageFormatConfig {
            width: w,
            height: h,
            format,
            ..Default::default()
        };
        compute_stride_align(&mut f, 64);
        f
    }

    fn template(out1: (u16, u16)) -> BackEnd {
        be_template(
            fmt(1280, 800, formats::BAYER16),
            BayerOrder::Bggr,
            64.0 / 1024.0,
            [
                Some(fmt(1280, 800, formats::NV12)),
                Some(fmt(out1.0, out1.1, formats::NV12)),
            ],
        )
        .unwrap()
    }

    const BOTH: u32 = rgb_enable::downscale(1) | rgb_enable::resample(1);

    #[test]
    fn a_box_steps_by_two_with_two_equal_taps() {
        let mut be = template((640, 400));
        assert!(set_half_filter(&mut be, HalfFilter::Box, (1280, 800)));
        let t = be.prepare().unwrap();
        let c = &t.config;
        assert_eq!(c.global.rgb_enables & BOTH, rgb_enable::resample(1));
        let r = c.resample[1];
        assert_eq!((r.scale_factor_h, r.scale_factor_v), (2 << 12, 2 << 12));
        for phase in r.coef.as_chunks::<TAPS>().0 {
            assert_eq!(*phase, [0, 0, 512, 512, 0, 0]);
        }
        assert!(t.num_tiles >= 1);
    }

    #[test]
    fn the_downscaler_box_runs_alone_at_two() {
        let mut be = template((640, 400));
        assert!(set_half_filter(
            &mut be,
            HalfFilter::BoxDownscaler,
            (1280, 800)
        ));
        let t = be.prepare().unwrap();
        let c = &t.config;
        assert_eq!(c.global.rgb_enables & BOTH, rgb_enable::downscale(1));
        let d = c.downscale[1];
        assert_eq!((d.scale_factor_h, d.scale_factor_v), (2 << 12, 2 << 12));
    }

    #[test]
    fn other_sizes_and_lanczos_keep_the_smart_resize() {
        for (filter, out) in [
            (HalfFilter::Lanczos3, (640, 400)),
            (HalfFilter::Box, (320, 200)),
            (HalfFilter::Box, (640, 360)),
        ] {
            let mut be = template(out);
            assert!(!set_half_filter(&mut be, filter, (1280, 800)));
            let c = be.prepare().unwrap().config;
            assert!(c.global.rgb_enables & rgb_enable::resample(1) != 0);
            assert_ne!(c.resample[1].coef[2..4], [512, 512]);
        }
    }

    /// A box set for one crop gives way to the smart resize when the crop changes size, and
    /// comes back with it.
    #[test]
    fn a_crop_of_another_size_goes_back_to_the_smart_resize() {
        let mut be = template((640, 400));
        assert!(set_half_filter(&mut be, HalfFilter::Box, (1280, 800)));
        be.set_crop(
            1,
            lemnos_drivers_pisp::uapi::BeCropConfig {
                offset_x: 0,
                offset_y: 0,
                width: 960,
                height: 600,
            },
        );
        assert!(!set_half_filter(&mut be, HalfFilter::Box, (960, 600)));
        let c = be.clone().prepare().unwrap().config;
        let step = ((959u32 << 12) / 639) as u16;
        assert_eq!(c.resample[1].scale_factor_h, step);
        assert_ne!(c.resample[1].coef[2..4], [512, 512]);
        be.set_crop(1, Default::default());
        assert!(set_half_filter(&mut be, HalfFilter::Box, (1280, 800)));
        let c = be.prepare().unwrap().config;
        assert_eq!(c.resample[1].scale_factor_h, 2 << 12);
    }
}
