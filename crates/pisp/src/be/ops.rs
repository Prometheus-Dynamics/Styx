//! Image operations on the back end for a caller's images, memory to memory: one
//! [`BeOp`] per job, run by `device::BeImageDevice` on the caller's dma-bufs. These are what a
//! vision library lowers its ops onto: resample and area downscale (pyramids), a tone LUT, a
//! 5x5 filter, demosaic with a colour matrix. Each names its capability ([`CAPABILITIES`]) and
//! says how exact it is.
//!
//! Written from `pisp_be_config.h` and libpisp's back end, and run on the Raze. The exactness
//! statements are what the configuration fixes, with the rounding measured where each op says
//! so.

use super::{BackEnd, defaults};
use crate::format::formats;
use crate::uapi::{
    BE_GAMMA_LUT_SIZE, BE_RESAMPLE_FILTER_SIZE, BE_SHARPEN_FUNC_NUM_POINTS, BayerOrder,
    BeDownscaleExtra, BeGammaConfig, BeOutputFormatConfig, BeResampleExtra, BeSharpenConfig,
    ImageFormatConfig, SharpenKernel, bayer_enable, image_format, rgb_enable,
};
#[cfg(not(feature = "std"))]
use styx_math::Float as _;

/// Polyphase resample on output 0 with the caller's phase coefficients ([`resample`]).
pub const CAP_RESAMPLE: &str = "styx.pisp.resample";
/// Area (box) downscale by a whole factor 2 to 8 on output 1 ([`downscale`]).
pub const CAP_DOWNSCALE: &str = "styx.pisp.downscale";
/// A non-decreasing 8-bit tone LUT through the gamma block ([`lut`]).
pub const CAP_LUT: &str = "styx.pisp.lut";
/// One 5x5 int8 kernel of the sharpen block, on luma ([`filter5x5`]).
pub const CAP_FILTER5X5: &str = "styx.pisp.filter5x5";
/// Bayer demosaic, then a 3x3 colour matrix with offsets ([`demosaic_ccm`]).
pub const CAP_DEMOSAIC_CCM: &str = "styx.pisp.demosaic_ccm";

/// 2x2 area halving on output 0, geometry exact, values within 1: the resampler as a box
/// at a fixed 2.0 step
/// ([`box2`]).
pub const CAP_BOX2: &str = "styx.pisp.box2";
/// Two pyramid levels in one job: [`box2`] on output 0 and a 4x area downscale on output 1,
/// both from the input ([`pyramid2`]).
pub const CAP_PYRAMID2: &str = "styx.pisp.pyramid2";

/// Every capability this module configures.
pub const CAPABILITIES: [&str; 7] = [
    CAP_RESAMPLE,
    CAP_DOWNSCALE,
    CAP_LUT,
    CAP_FILTER5X5,
    CAP_DEMOSAIC_CCM,
    CAP_BOX2,
    CAP_PYRAMID2,
];

/// A caller image's pixel layout (input and output alike).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pixels {
    /// 8-bit grey (`GREY`) out, from the luma plane of an `NV12` input: the input buffer has
    /// room for the chroma after it ([`crate::device::BeImageDevice::input_len`]), which is not
    /// read into the result and need not be written.
    Grey8,
    /// Packed R, G, B bytes (`RGB3`).
    Rgb888,
    /// Planar YUV 4:2:0 (`YU12`). The ops act on each plane as a channel (a LUT maps chroma
    /// too).
    Yuv420p,
    /// NV12, as [`Pixels::Yuv420p`].
    Nv12,
}

impl Pixels {
    /// The PiSP image format.
    pub fn pisp_format(self) -> u32 {
        match self {
            Self::Grey8 => image_format::BPS_8,
            Self::Rgb888 => formats::RGB888,
            Self::Yuv420p => formats::YUV420P,
            Self::Nv12 => formats::NV12,
        }
    }

    /// The V4L2 pixel format.
    pub fn fourcc(self) -> [u8; 4] {
        *match self {
            Self::Grey8 => b"GREY",
            Self::Rgb888 => b"RGB3",
            Self::Yuv420p => b"YU12",
            Self::Nv12 => b"NV12",
        }
    }
}

/// One job's configuration: the back end, which output it writes, and the images' formats
/// (strides left at zero; the device fills them in from the nodes).
#[derive(Clone, Debug)]
pub struct BeOp {
    /// The configuration.
    pub backend: BackEnd,
    /// The output the result is on (0 or 1).
    pub output: usize,
    /// The input image.
    pub input: ImageFormatConfig,
    /// Its V4L2 pixel format.
    pub input_fourcc: [u8; 4],
    /// The output image.
    pub out: ImageFormatConfig,
    /// Its V4L2 pixel format.
    pub out_fourcc: [u8; 4],
    /// A second image on output 1, in the same pixel format, when the op writes both outputs
    /// (`output` is then 0).
    pub out1: Option<ImageFormatConfig>,
}

impl BeOp {
    /// Sets the input and output formats (with the strides the nodes chose) in the config.
    pub fn set_formats(&mut self, input: ImageFormatConfig, out: ImageFormatConfig) {
        self.input = input;
        self.out = out;
        self.backend.set_input_format(input);
        self.backend.set_output_format(
            self.output,
            BeOutputFormatConfig {
                image: out,
                ..Default::default()
            },
        );
    }

    /// Sets the second output's format (with the stride its node chose) in the config.
    pub fn set_out1_format(&mut self, out1: ImageFormatConfig) {
        self.out1 = Some(out1);
        self.backend.set_output_format(
            1,
            BeOutputFormatConfig {
                image: out1,
                ..Default::default()
            },
        );
    }
}

/// A size, factor or curve the block does not take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpError {
    /// Smaller than the back end's 16x16 minimum.
    TooSmall(u16, u16),
    /// A downscale factor outside 2 to 8 or not dividing the size.
    Factor(u16),
    /// The gamma block takes non-decreasing curves only.
    Decreasing,
    /// The sharpen block works on luma: RGB input has none.
    NoLuma,
    /// Demosaic needs a colour Bayer order.
    NotColour,
}

impl core::fmt::Display for OpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooSmall(w, h) => write!(f, "{w}x{h}: the back end takes 16x16 and up"),
            Self::Factor(k) => write!(f, "downscale by {k}: 2 to 8, dividing the size"),
            Self::Decreasing => f.write_str("the gamma block takes non-decreasing curves only"),
            Self::NoLuma => f.write_str("the sharpen block works on luma: give YUV or grey"),
            Self::NotColour => f.write_str("demosaic needs a colour Bayer order"),
        }
    }
}

impl core::error::Error for OpError {}

fn check_size((w, h): (u16, u16)) -> Result<(), OpError> {
    if w < 16 || h < 16 {
        return Err(OpError::TooSmall(w, h));
    }
    Ok(())
}

fn image((width, height): (u16, u16), format: u32) -> ImageFormatConfig {
    ImageFormatConfig {
        width,
        height,
        format,
        ..Default::default()
    }
}

/// A job reading `pixels` (`size`) and writing them to output `output` at `out`, every block
/// between off: the image passes unchanged but for what the op enables.
///
/// Grey goes in as the luma plane of an `NV12` image, its chroma unread by a `GREY` output. 8-bit
/// uncompressed input on the Bayer path stalls the back end, and the RGB input always reads
/// three interleaved channels, whatever the format says.
fn passthrough(pixels: Pixels, size: (u16, u16), output: usize, out: (u16, u16)) -> BeOp {
    let input = if pixels == Pixels::Grey8 {
        Pixels::Nv12
    } else {
        pixels
    };
    let mut backend = BackEnd::new();
    backend.set_global(
        0,
        rgb_enable::INPUT | rgb_enable::output(output),
        BayerOrder::Rggb,
    );
    let mut op = BeOp {
        backend,
        output,
        input: image(size, input.pisp_format()),
        input_fourcc: input.fourcc(),
        out: image(out, pixels.pisp_format()),
        out_fourcc: pixels.fourcc(),
        out1: None,
    };
    op.set_formats(op.input, op.out);
    op
}

/// Resample `pixels` (`size`) to `out` on output 0 with `coef`: 16 phases of 6 taps in s.10
/// (1024 = 1.0; each phase's taps sum to 1024 for unit gain), phase-major.
///
/// Exactness: the filter the coefficients describe, sampled at 1/16-pixel phases, so a
/// reference at exact phases differs by the phase rounding (up to 1/32 pixel); the step is a
/// 4.12 fraction. Horizontal then vertical, clamped to range between the passes, truncated at
/// the end: that model is within 1 on the Raze, 2026-11-03, Lanczos 3 from 640x480 to half
/// (2979 of 76800 differ).
/// Scale factors under 16 either way.
pub fn resample(
    pixels: Pixels,
    size: (u16, u16),
    out: (u16, u16),
    coef: &[i16; BE_RESAMPLE_FILTER_SIZE],
) -> Result<BeOp, OpError> {
    check_size(size)?;
    check_size(out)?;
    let mut op = passthrough(pixels, size, 0, out);
    op.backend.set_resample(
        0,
        BeResampleExtra {
            scaled_width: out.0,
            scaled_height: out.1,
            ..Default::default()
        },
    );
    let cfg = op.backend.config_mut();
    cfg.resample[0].coef = *coef;
    cfg.global.rgb_enables |= rgb_enable::resample(0);
    Ok(op)
}

/// Downscale `pixels` (`size`) by `factor` (2 to 8, both ways) on output 1, the output with
/// the downscaler: each output pixel the mean of a `factor` x `factor` block.
///
/// Exactness: an area mean over whole, non-overlapping blocks (`size` a multiple of
/// `factor`), truncated: Raze, 2026-11-03, by 2 at 640x480, never above the mean rounded half up
/// and at most 1 below it. [`pyramid2`] writes two levels in one job.
pub fn downscale(pixels: Pixels, size: (u16, u16), factor: u16) -> Result<BeOp, OpError> {
    check_size(size)?;
    if !(2..=8).contains(&factor)
        || !size.0.is_multiple_of(factor)
        || !size.1.is_multiple_of(factor)
    {
        return Err(OpError::Factor(factor));
    }
    let out = (size.0 / factor, size.1 / factor);
    let mut op = passthrough(pixels, size, 1, out);
    op.backend.set_downscale(
        1,
        BeDownscaleExtra {
            scaled_width: out.0,
            scaled_height: out.1,
        },
    );
    op.backend.config_mut().global.rgb_enables |= rgb_enable::downscale(1);
    Ok(op)
}

/// The gamma block's knot `i` on the 16-bit scale: 32 knots 512 apart, then 16 knots 1024
/// apart, then 16 knots 2048 apart (the last segment ends at 65536). On 8-bit codes (x 257),
/// about every 2nd code below 64, every 4th to 128, every 8th above.
pub fn gamma_knot(i: usize) -> u32 {
    let i = i as u32;
    if i < 32 {
        i * 512
    } else if i < 48 {
        (i - 32) * 1024 + 16384
    } else {
        (i - 48) * 2048 + 32768
    }
}

/// How a LUT fits the gamma block ([`lut`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LutFit {
    /// Codes whose output differs from the table.
    pub inexact_codes: u16,
    /// The largest difference, in 8-bit levels.
    pub max_error: u8,
}

/// A tone LUT through the gamma block: `table[v]` for every 8-bit `v`, on every channel.
///
/// Exactness: the block is piecewise linear between its 64 knots ([`gamma_knot`]) with
/// unsigned 14-bit rises, so it is non-decreasing and climbs at most 63 8-bit levels per
/// segment; the table is met at the knots and interpolated between them. [`LutFit`] says,
/// for the block as configured and its interpolation rounded to nearest, which codes miss
/// and by how much: none for a table linear between knots, the codes around the step for a
/// threshold. A decreasing table is refused.
pub fn lut(pixels: Pixels, size: (u16, u16), table: &[u8; 256]) -> Result<(BeOp, LutFit), OpError> {
    check_size(size)?;
    if table.windows(2).any(|w| w[1] < w[0]) {
        return Err(OpError::Decreasing);
    }
    // The table as a curve on the 16-bit scales (code c at c * 257).
    let points: [(u32, u32); 256] =
        core::array::from_fn(|c| (c as u32 * 257, u32::from(table[c]) * 257));
    let mut gamma = defaults::gamma_from_curve(&points);
    // The last knot (code 247) has no next one to rise to: rise to the top code over the
    // last segment's 2048.
    let last = gamma.lut[BE_GAMMA_LUT_SIZE - 1];
    let rise = (u32::from(table[255]) * 257).saturating_sub(last & 0xffff);
    gamma.lut[BE_GAMMA_LUT_SIZE - 1] = last | rise.min((1 << 14) - 1) << 16;
    let fit = fit(table, &gamma);
    let mut op = passthrough(pixels, size, 0, size);
    op.backend.config_mut().gamma = gamma;
    op.backend.config_mut().global.rgb_enables |= rgb_enable::GAMMA;
    Ok((op, fit))
}

/// The block's output at every code against the table: each knot's value plus its rise
/// times the position in the segment, rounded to an 8-bit level.
fn fit(table: &[u8; 256], gamma: &BeGammaConfig) -> LutFit {
    let mut out = LutFit {
        inexact_codes: 0,
        max_error: 0,
    };
    for (code, &want) in table.iter().enumerate() {
        let x = code as u32 * 257;
        let i = (0..BE_GAMMA_LUT_SIZE)
            .rev()
            .find(|&i| gamma_knot(i) <= x)
            .unwrap_or(0);
        let x0 = gamma_knot(i);
        let width = if i + 1 < BE_GAMMA_LUT_SIZE {
            gamma_knot(i + 1) - x0
        } else {
            2048
        };
        let entry = gamma.lut[i];
        let y = (entry & 0xffff) * width + (entry >> 16) * (x - x0);
        // y / width on the 16-bit scale, then / 257 to a level, rounded to nearest.
        let got = (2 * y + 257 * width) / (2 * 257 * width);
        let err = got.abs_diff(u32::from(want)).min(255) as u8;
        if err > 0 {
            out.inexact_codes += 1;
            out.max_error = out.max_error.max(err);
        }
    }
    out
}

/// One 5x5 int8 `kernel` of the sharpen block on luma, the other four off: each sample gets
/// the kernel's response added at unit gain, `clamp(y + sum(k * neighbourhood))`. A high-pass
/// kernel sums to 0; a smoothing filter is `blur - identity`, in whole coefficients.
///
/// Unit gain is `func` 4096, strength 256, scale 1024. `func` multiplies the response, it is
/// not a transfer curve, and a scale of 4096 reads as 0. Lower gains are not measured to hold:
/// the same product split differently came out up to 62 off on a strong kernel. `white` and
/// `black` stay 0: set, they force samples to 255 and 0.
///
/// Exactness: exact on the Raze, 2026-11-03, GREY 640x480: a zero kernel passes all 307200
/// samples, and two high-passes equal the clamped sum on all 302736 samples clear of the 2-pixel
/// border (the border is not measured). Luma only: chroma passes unchanged.
pub fn filter5x5(
    pixels: Pixels,
    size: (u16, u16),
    kernel: &SharpenKernel,
) -> Result<BeOp, OpError> {
    check_size(size)?;
    if pixels == Pixels::Rgb888 {
        return Err(OpError::NoLuma);
    }
    let mut op = passthrough(pixels, size, 0, size);
    let unit = [4096; BE_SHARPEN_FUNC_NUM_POINTS];
    let cfg = op.backend.config_mut();
    cfg.sharpen = BeSharpenConfig {
        kernel0: *kernel,
        thresholds: [[0, 0, 1024, 0]; 5],
        positive_strength: 1 << 8,
        positive_pre_limit: u16::MAX,
        positive_func: unit,
        positive_limit: u16::MAX,
        negative_strength: 1 << 8,
        negative_pre_limit: u16::MAX,
        negative_func: unit,
        negative_limit: u16::MAX,
        enables: 1,
        ..Default::default()
    };
    cfg.sh_fc_combine.y_factor = u8::MAX;
    cfg.global.rgb_enables |= rgb_enable::SHARPEN;
    Ok(op)
}

/// Demosaic a 16-bit Bayer image (`order`, `size`) and apply `ccm` (row-major, 1.0 = 1.0)
/// then `offsets` (8-bit levels), out as RGB888 on output 0.
///
/// Exactness: the demosaic is the hardware's own (edge-directed, not bilinear): a tolerance,
/// owed a CM5 measurement against a bilinear reference. The matrix is fixed point, each
/// coefficient rounded to 1/1024 (in -8 to 8), the offsets to 1/1024 of a 16-bit level.
pub fn demosaic_ccm(
    order: BayerOrder,
    size: (u16, u16),
    ccm: [f64; 9],
    offsets: [f64; 3],
) -> Result<BeOp, OpError> {
    check_size(size)?;
    let bayer = *match order {
        BayerOrder::Rggb => b"RG16",
        BayerOrder::Gbrg => b"GB16",
        BayerOrder::Bggr => b"BYR2",
        BayerOrder::Grbg => b"GR16",
        BayerOrder::Greyscale => return Err(OpError::NotColour),
    };
    let mut backend = BackEnd::new();
    backend.set_global(
        bayer_enable::INPUT | bayer_enable::DEMOSAIC,
        rgb_enable::CCM | rgb_enable::OUTPUT0,
        order,
    );
    backend.set_ccm(ccm);
    // 16.10: an 8-bit level is 257 on the 16-bit scale, shifted 10.
    backend.config_mut().ccm.offsets = offsets.map(|o| (o * 257.0 * 1024.0).round() as i32);
    let mut op = BeOp {
        backend,
        output: 0,
        input: image(size, formats::BAYER16),
        input_fourcc: bayer,
        out: image(size, formats::RGB888),
        out_fourcc: *b"RGB3",
        out1: None,
    };
    op.set_formats(op.input, op.out);
    Ok(op)
}

mod pyramid;
pub use pyramid::{BOX2, box2, pyramid2};

#[cfg(test)]
mod tests;
