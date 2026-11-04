//! Pyramid levels: an exact 2x2 area halving through the resampler, and two levels in one
//! job (the halving on output 0, a 4x area downscale on output 1, both from the input; the
//! back end's two branches tap the same point, so the second level cannot come from the
//! first).

use super::{BeOp, OpError, Pixels, check_size, image, passthrough};
use crate::uapi::{BE_RESAMPLE_FILTER_SIZE, BeDownscaleExtra, BeResampleExtra, rgb_enable};

/// The resampler's taps for [`box2`]: every phase `[0, 0, 512, 512, 0, 0]` (s.10), the
/// input pixel at the output position (tap 2) and the next one at half weight each.
pub const BOX2: [i16; BE_RESAMPLE_FILTER_SIZE] = {
    let mut c = [0i16; BE_RESAMPLE_FILTER_SIZE];
    let mut p = 0;
    while p < BE_RESAMPLE_FILTER_SIZE / 6 {
        c[p * 6 + 2] = 512;
        c[p * 6 + 3] = 512;
        p += 1;
    }
    c
};

const STEP_2: u16 = 2 << 12;

/// Halve `pixels` (`size`, both even) on output 0: each output pixel the mean of the
/// block-aligned 2x2 block under it, no phase shift.
///
/// Exactness: the geometry is exact. The step is exactly 2.0 (fixed, not corner-aligned),
/// so output `x` sits on input `2x` at phase 0 for every pixel and tile, and the taps weigh
/// `x` and `x + 1` by 512/1024 each: the block-aligned 2x2 mean, no shift. The value is
/// within 1 of `(a + b + c + d + 2) >> 2`: 8-bit samples enter the 16-bit pipeline as
/// `v * 257`, and the result is truncated: on the Raze, 2026-11-03, never above that value and
/// at most 1 below it. Luma (and
/// grey) only; the chroma of YUV inputs passes through the back end's 4:4:4 stage and is
/// not claimed.
pub fn box2(pixels: Pixels, size: (u16, u16)) -> Result<BeOp, OpError> {
    check_size(size)?;
    if !size.0.is_multiple_of(2) || !size.1.is_multiple_of(2) {
        return Err(OpError::Factor(2));
    }
    let out = (size.0 / 2, size.1 / 2);
    let mut op = passthrough(pixels, size, 0, out);
    op.backend.set_resample(
        0,
        BeResampleExtra {
            scaled_width: out.0,
            scaled_height: out.1,
            ..Default::default()
        },
    );
    op.backend.set_resample_step(0, Some((STEP_2, STEP_2)));
    let cfg = op.backend.config_mut();
    cfg.resample[0].coef = BOX2;
    cfg.global.rgb_enables |= rgb_enable::resample(0);
    Ok(op)
}

/// Two pyramid levels of `pixels` (`size`, both multiples of 4) in one job: [`box2`] on
/// output 0 (`out`) and a 4x area downscale on output 1 (`out1`, a quarter of `size`).
///
/// Exactness: output 0 as [`box2`]. Output 1 is the 4x4 block mean of the input, which
/// differs from halving output 0 again by at most 1 (that rounds twice); truncated as
/// `downscale`'s.
pub fn pyramid2(pixels: Pixels, size: (u16, u16)) -> Result<BeOp, OpError> {
    if !size.0.is_multiple_of(4) || !size.1.is_multiple_of(4) {
        return Err(OpError::Factor(4));
    }
    let mut op = box2(pixels, size)?;
    let quarter = (size.0 / 4, size.1 / 4);
    op.backend.set_downscale(
        1,
        BeDownscaleExtra {
            scaled_width: quarter.0,
            scaled_height: quarter.1,
        },
    );
    op.backend.config_mut().global.rgb_enables |= rgb_enable::downscale(1) | rgb_enable::output(1);
    op.set_out1_format(image(quarter, pixels.pisp_format()));
    Ok(op)
}
