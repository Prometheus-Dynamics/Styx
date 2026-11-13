//! How the algorithms' parameters become the neutral ISP settings the loop hands out.
//!
//! The settings type is [`styx_settings::IspSettings`] (re-exported from this crate). It is
//! defined there, so the controller and every ISP share one value. The loop fills its own kept
//! value in place with the functions below, so a frame allocates nothing once the buffers have
//! grown.

use alloc::vec;

use styx_aaa::{LensShading, Params};
use styx_settings::IspSettings;

/// Sets `isp` to what `p` (computed from frame `frame`) asks for, with `digital_gain` for the
/// frame being processed. Reuses `isp`'s curve and table buffers. The controller never sets
/// `outputs`: they stay as they are (empty).
pub(crate) fn set_from_params(isp: &mut IspSettings, p: &Params, frame: u64, digital_gain: f64) {
    let g = p.colour_gains[1].max(1e-6);
    // Every field is set here (a field left out would keep the old frame's value).
    let cfa = p.black_level.cfa();
    isp.from_frame = frame;
    isp.black_level = cfa.into_iter().fold(f64::INFINITY, f64::min);
    isp.black_channels = cfa.iter().any(|v| *v != cfa[0]).then_some(cfa);
    isp.wb = [p.colour_gains[0] / g, 1.0, p.colour_gains[2] / g];
    isp.digital_gain = digital_gain * g;
    isp.flicker = 1.0;
    isp.flicker_bands = None;
    isp.ccm = p.ccm;
    isp.gamma.clone_from(&p.gamma);
    isp.lens_shading.clone_from(&p.lens_shading);
    isp.denoise = p.denoise;
    isp.sharpen = p.sharpen;
    isp.histogram_weights.clone_from(&p.histogram_weights);
}

/// Scales the spatial (SDN) and colour (CDN) denoise thresholds of `isp` by `scale` (both off at
/// 0; 1 leaves them as tuned).
pub(crate) fn scale_spatial_denoise(isp: &mut IspSettings, scale: f64) {
    if scale == 1.0 {
        return;
    }
    let d = &mut isp.denoise;
    if scale <= 0.0 {
        (d.sdn, d.cdn) = (None, None);
    }
    if let Some(s) = &mut d.sdn {
        s.noise_constant *= scale;
        s.noise_slope *= scale;
        s.noise_constant2 *= scale;
        s.noise_slope2 *= scale;
    }
    if let Some(c) = &mut d.cdn {
        c.threshold *= scale;
    }
}

/// The lens shading table with deflicker's band gains folded in: one gain per band of rows
/// (top to bottom), interpolated linearly between band centres and applied to every row of the
/// table. Without a table, a unity table of the bands' height.
pub(crate) fn lens_shading_with_bands(ls: Option<&LensShading>, bands: &[f64]) -> LensShading {
    let mut out = ls.cloned().unwrap_or_else(|| {
        let n = 2 * bands.len().max(2);
        LensShading {
            width: 2,
            height: bands.len().max(2) as u32,
            r: vec![1.0; n],
            g: vec![1.0; n],
            b: vec![1.0; n],
            generation: 0,
        }
    });
    let (w, h) = (out.width as usize, out.height as usize);
    if bands.is_empty() || w == 0 || h == 0 {
        return out;
    }
    // The gains change below: these are no longer `ls`'s tables.
    out.generation = 0;
    for y in 0..h {
        // The band gain at the grid row's centre, linearly between band centres.
        let pos = ((y as f64 + 0.5) / h as f64 * bands.len() as f64 - 0.5)
            .clamp(0.0, (bands.len() - 1) as f64);
        let i = (pos as usize).min(bands.len() - 1);
        let next = bands[(i + 1).min(bands.len() - 1)];
        let g = bands[i] + (next - bands[i]) * (pos - i as f64);
        for t in [&mut out.r, &mut out.g, &mut out.b] {
            for v in t.iter_mut().skip(y * w).take(w) {
                *v *= g;
            }
        }
    }
    out
}
