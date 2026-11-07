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
//! Levels 2 and 3 from extra passes on output 1's downscaler (Raze #1, 145 live frames,
//! 2026-11-07), against the area mean of the full luma (the CPU's chained 2x2 box rounds at
//! every level, so it drifts further: 74.6 % and 63.1 % identical, max 4 and 2):
//!
//! | level | identical | \|diff\| <= 1 | max \|diff\| | mean (PiSP - CPU) |
//! |---|---|---|---|---|
//! | 2 (320x200) | 90.6 % | 100.0 % | 3 | -0.04 |
//! | 3 (160x100) | 94.1 % | 100.0 % | 1 | -0.02 |
//!
//! Before, the passes ran on output 0, which has no downscaler, and fell back to the smart
//! resize: 20.8 % and 10.6 % identical, max 65 and 111 (a scale error of up to 3/8 pixel).
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
    if (u32::from(w) * 2, u32::from(h) * 2) != (u32::from(input.0), u32::from(input.1)) {
        be.set_resample_step(1, None);
        be.set_smart_resize(1, w, h);
        return false;
    }
    set_box(be, 1, filter, input, (w, h)).is_some()
}

/// Output `o` making `size` from `input` (its crop) as a box average when `size` is exactly
/// `input` / k for k = 2, 4 or 8 and `filter` is a box: on output 1, k = 2 as `filter` says and
/// k = 4 and 8 from the downscaler alone (an area average of k x k; the CPU's repeated 2x2 box
/// gives the same mean). Output 0 has no downscaler: its resampler steps by exactly k with k
/// equal taps for k = 2 and 4; k = 8 would need 8 taps of its 6. Anything else gets the smart
/// resize (Lanczos 3, corner aligned: at 4x it lands up to 3/8 of a pixel off the box at the
/// edges). Returns k when the box was set.
pub fn set_box(
    be: &mut BackEnd,
    o: usize,
    filter: HalfFilter,
    input: (u16, u16),
    size: (u16, u16),
) -> Option<u32> {
    let (w, h) = size;
    be.set_resample_step(o, None);
    let k = if w == 0 {
        0
    } else {
        u32::from(input.0) / u32::from(w)
    };
    let exact =
        k > 0 && (u32::from(w) * k, u32::from(h) * k) == (u32::from(input.0), u32::from(input.1));
    let downscaler = o == 1 && (k >= 4 || filter == HalfFilter::BoxDownscaler);
    let taps = !downscaler && matches!(k, 2 | 4) && (o == 0 || filter == HalfFilter::Box);
    if filter == HalfFilter::Lanczos3 || !exact || !(downscaler || taps) {
        be.set_smart_resize(o, w, h);
        return None;
    }
    be.set_smart_resize(o, 0, 0);
    let mut rgb =
        be.config().global.rgb_enables & !(rgb_enable::downscale(o) | rgb_enable::resample(o));
    if downscaler {
        be.set_downscale(
            o,
            BeDownscaleExtra {
                scaled_width: w,
                scaled_height: h,
            },
        );
        rgb |= rgb_enable::downscale(o);
    } else {
        be.set_resample(
            o,
            BeResampleExtra {
                scaled_width: w,
                scaled_height: h,
                ..Default::default()
            },
        );
        // Output pixel x at input 2x (4x): the taps from the centre on are its k pixels.
        let step = (k << 12) as u16;
        be.set_resample_step(o, Some((step, step)));
        let tap = ONE / k as i16;
        let mut coef = [0i16; 96];
        for phase in coef.as_chunks_mut::<TAPS>().0 {
            phase[CENTRE..CENTRE + k as usize].fill(tap);
        }
        be.config_mut().resample[o].coef = coef;
        rgb |= rgb_enable::resample(o);
    }
    be.config_mut().global.rgb_enables = rgb;
    Some(k)
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

    #[test]
    fn quarter_and_eighth_boxes_come_from_the_downscaler_alone() {
        for (k, out) in [(4u16, (320, 200)), (8, (160, 100))] {
            for filter in [HalfFilter::Box, HalfFilter::BoxDownscaler] {
                let mut be = template(out);
                assert_eq!(
                    set_box(&mut be, 1, filter, (1280, 800), out),
                    Some(u32::from(k))
                );
                let c = be.prepare().unwrap().config;
                assert_eq!(c.global.rgb_enables & BOTH, rgb_enable::downscale(1));
                let d = c.downscale[1];
                let step = u32::from(k) << 12;
                assert_eq!(
                    (u32::from(d.scale_factor_h), u32::from(d.scale_factor_v)),
                    (step, step)
                );
            }
        }
    }

    /// Output 0 (no downscaler) at 4x: the resampler steps by 4 with four equal taps, so
    /// output pixel x is the mean of input pixels 4x..4x + 3 on each axis.
    #[test]
    fn output_0_makes_a_quarter_box_with_four_taps() {
        let mut be = template((640, 400));
        assert_eq!(
            set_box(&mut be, 0, HalfFilter::Box, (1280, 800), (320, 200)),
            Some(4)
        );
        // Output 0 at 320x200, as an extra pass sets its format.
        let mut out = be.config().output_format[0];
        out.image = fmt(320, 200, formats::NV12);
        be.set_output_format(0, out);
        let c = be.prepare().unwrap().config;
        assert_eq!(
            c.global.rgb_enables & rgb_enable::resample(0),
            rgb_enable::resample(0)
        );
        let r = c.resample[0];
        assert_eq!((r.scale_factor_h, r.scale_factor_v), (4 << 12, 4 << 12));
        for phase in r.coef.as_chunks::<TAPS>().0 {
            assert_eq!(*phase, [0, 0, 256, 256, 256, 256]);
        }
    }

    #[test]
    fn output_0_has_no_downscaler_and_lanczos_never_boxes() {
        let mut be = template((640, 400));
        assert_eq!(
            set_box(&mut be, 0, HalfFilter::Box, (1280, 800), (160, 100)),
            None
        );
        // Asked for the downscaler where there is none: the resampler's box.
        assert_eq!(
            set_box(
                &mut be,
                0,
                HalfFilter::BoxDownscaler,
                (1280, 800),
                (640, 400)
            ),
            Some(2)
        );
        assert_eq!(
            set_box(&mut be, 1, HalfFilter::Lanczos3, (1280, 800), (320, 200)),
            None
        );
        assert_eq!(
            set_box(&mut be, 1, HalfFilter::Box, (1280, 800), (400, 250)),
            None
        );
    }
}
