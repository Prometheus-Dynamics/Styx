//! Interpolating between two tunings, as between the tunings at two colour temperatures.
//!
//! The rule per field, for a weight `t` in `(0, 1)` (`t = 0` is `self`, `t = 1` is `other`, and
//! `t` is clamped to `[0, 1]`; NaN counts as 0; the endpoints return the inputs unchanged):
//!
//! * Scalars (`black_level`, `digital_gain`, `flicker`, the gains, the colour matrix entries, the
//!   denoise noise constants and slopes, the sharpening factors, the black level of each channel)
//!   are blended linearly: `(1 - t) * self + t * other`. Matrices blend entry by entry, which is the
//!   usual way to move between two colour matrices; it is not a colour-science interpolation.
//! * Tables (`flicker_bands`, the lens shading gains, the histogram weights) are blended entry by
//!   entry when both sides have the same length (the same grid). When the shapes differ, the
//!   nearer side is taken whole.
//! * Tone curves (`gamma`) are blended over the union of both curves' breakpoints, evaluating each
//!   curve (with its own extrapolation) at every breakpoint. The result is exact between the two
//!   curves' knots, and it can have more breakpoints than either input.
//! * Option-valued blocks (`sdn`, `cdn`, `tdn`, `geq`, `sharpen`, `gamma`, `lens_shading`,
//!   `histogram_weights`, `flicker_bands`, `black_channels`) are blended when both tunings set them.
//!   When one side leaves one out, the nearer side's value is taken whole: a block is on or off, and
//!   a blend of on and off is not a value. The nearer side is `self` for `t < 0.5`, else `other`.
//! * Discrete fields are never blended, the nearer side's value is taken: `from_frame` (provenance),
//!   `denoise.dpc` (its mode is 0, 1 or 2).
//! * `outputs` (sizes, pixel formats, colour spaces and ranges) are never blended. They are what
//!   a consumer asked for, not a tuned quantity: the nearer side's list is taken whole.

use alloc::vec::Vec;

use styx_aaa::{
    CdnParams, DenoiseParams, GeqParams, LensShading, Pwl, SdnParams, SharpenParams, TdnParams,
    ZoneGrid,
};

use crate::IspSettings;

impl IspSettings {
    /// These settings blended with `other` by `t` (see the module's rule per field). Returns a
    /// copy of `self` at `t = 0` and of `other` at `t = 1`.
    pub fn interpolate(&self, other: &Self, t: f64) -> Self {
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        if t <= 0.0 {
            return self.clone();
        }
        if t >= 1.0 {
            return other.clone();
        }
        let (a, b) = (self, other);
        let near = if t < 0.5 { a } else { b };
        let mut wb = [0.0; 3];
        for (o, (x, y)) in wb.iter_mut().zip(a.wb.iter().zip(b.wb)) {
            *o = mix(t, *x, y);
        }
        let mut ccm = [0.0; 9];
        for (o, (x, y)) in ccm.iter_mut().zip(a.ccm.iter().zip(b.ccm)) {
            *o = mix(t, *x, y);
        }
        IspSettings {
            from_frame: near.from_frame,
            black_level: mix(t, a.black_level, b.black_level),
            black_channels: match (a.black_channels, b.black_channels) {
                (None, None) => None,
                _ => {
                    let (x, y) = (a.black_cfa(), b.black_cfa());
                    Some(core::array::from_fn(|i| mix(t, x[i], y[i])))
                }
            },
            wb,
            digital_gain: mix(t, a.digital_gain, b.digital_gain),
            flicker: mix(t, a.flicker, b.flicker),
            flicker_bands: match (&a.flicker_bands, &b.flicker_bands) {
                (Some(x), Some(y)) if x.len() == y.len() => {
                    Some(x.iter().zip(y).map(|(p, q)| mix(t, *p, *q)).collect())
                }
                _ => near.flicker_bands.clone(),
            },
            ccm,
            gamma: match (&a.gamma, &b.gamma) {
                (Some(x), Some(y)) => Some(blend_curves(x, y, t, near.gamma.as_ref())),
                _ => near.gamma.clone(),
            },
            lens_shading: match (&a.lens_shading, &b.lens_shading) {
                (Some(x), Some(y)) => blend_shading(x, y, t).or_else(|| near.lens_shading.clone()),
                _ => near.lens_shading.clone(),
            },
            denoise: blend_denoise(&a.denoise, &b.denoise, t, &near.denoise),
            sharpen: match (a.sharpen, b.sharpen) {
                (Some(x), Some(y)) => Some(blend_sharpen(&x, &y, t)),
                _ => near.sharpen,
            },
            histogram_weights: match (&a.histogram_weights, &b.histogram_weights) {
                (Some(x), Some(y)) => {
                    blend_grid(x, y, t).or_else(|| near.histogram_weights.clone())
                }
                _ => near.histogram_weights.clone(),
            },
            outputs: near.outputs.clone(),
        }
    }
}

/// `(1 - t) * x + t * y`: exactly `x` at `t = 0` and exactly `y` at `t = 1`. Equal inputs give
/// that value exactly at every `t` (no rounding), so a value both tunings share stays unchanged.
fn mix(t: f64, x: f64, y: f64) -> f64 {
    if x == y { x } else { (1.0 - t) * x + t * y }
}

fn blend_curves(a: &Pwl, b: &Pwl, t: f64, near: Option<&Pwl>) -> Pwl {
    let fallback = || near.cloned().unwrap_or_else(|| a.clone());
    if a.is_empty() || b.is_empty() {
        return fallback();
    }
    let mut xs: Vec<f64> = a.points().iter().chain(b.points()).map(|p| p.0).collect();
    xs.sort_unstable_by(f64::total_cmp);
    xs.dedup();
    let points = xs
        .into_iter()
        .map(|x| (x, mix(t, a.eval(x), b.eval(x))))
        .collect();
    Pwl::new(points).unwrap_or_else(|_| fallback())
}

fn blend_shading(a: &LensShading, b: &LensShading, t: f64) -> Option<LensShading> {
    let same = a.width == b.width
        && a.height == b.height
        && a.r.len() == b.r.len()
        && a.g.len() == b.g.len()
        && a.b.len() == b.b.len();
    if !same {
        return None;
    }
    let table = |x: &[f64], y: &[f64]| -> Vec<f64> {
        x.iter().zip(y).map(|(p, q)| mix(t, *p, *q)).collect()
    };
    Some(LensShading {
        width: a.width,
        height: a.height,
        r: table(&a.r, &b.r),
        g: table(&a.g, &b.g),
        b: table(&a.b, &b.b),
        // New gains: no generation shared with either input.
        generation: 0,
    })
}

fn blend_grid(a: &ZoneGrid<f64>, b: &ZoneGrid<f64>, t: f64) -> Option<ZoneGrid<f64>> {
    if a.width != b.width || a.height != b.height || a.zones.len() != b.zones.len() {
        return None;
    }
    Some(ZoneGrid {
        width: a.width,
        height: a.height,
        zones: a
            .zones
            .iter()
            .zip(&b.zones)
            .map(|(p, q)| mix(t, *p, *q))
            .collect(),
    })
}

fn blend_sharpen(a: &SharpenParams, b: &SharpenParams, t: f64) -> SharpenParams {
    SharpenParams {
        threshold: mix(t, a.threshold, b.threshold),
        strength: mix(t, a.strength, b.strength),
        limit: mix(t, a.limit, b.limit),
    }
}

fn blend_denoise(
    a: &DenoiseParams,
    b: &DenoiseParams,
    t: f64,
    near: &DenoiseParams,
) -> DenoiseParams {
    // Both sides set (`x`, `y`): blend; otherwise the nearer side's block, whole.
    fn block<T: Copy>(
        x: Option<T>,
        y: Option<T>,
        near: Option<T>,
        f: impl FnOnce(T, T) -> T,
    ) -> Option<T> {
        match (x, y) {
            (Some(p), Some(q)) => Some(f(p, q)),
            _ => near,
        }
    }
    DenoiseParams {
        noise_constant: mix(t, a.noise_constant, b.noise_constant),
        noise_slope: mix(t, a.noise_slope, b.noise_slope),
        sdn: block(a.sdn, b.sdn, near.sdn, |p: SdnParams, q: SdnParams| {
            SdnParams {
                noise_constant: mix(t, p.noise_constant, q.noise_constant),
                noise_slope: mix(t, p.noise_slope, q.noise_slope),
                noise_constant2: mix(t, p.noise_constant2, q.noise_constant2),
                noise_slope2: mix(t, p.noise_slope2, q.noise_slope2),
                strength: mix(t, p.strength, q.strength),
            }
        }),
        cdn: block(a.cdn, b.cdn, near.cdn, |p: CdnParams, q: CdnParams| {
            CdnParams {
                threshold: mix(t, p.threshold, q.threshold),
                strength: mix(t, p.strength, q.strength),
            }
        }),
        tdn: block(a.tdn, b.tdn, near.tdn, |p: TdnParams, q: TdnParams| {
            TdnParams {
                noise_constant: mix(t, p.noise_constant, q.noise_constant),
                noise_slope: mix(t, p.noise_slope, q.noise_slope),
                threshold: mix(t, p.threshold, q.threshold),
            }
        }),
        geq: block(a.geq, b.geq, near.geq, |p: GeqParams, q: GeqParams| {
            GeqParams {
                offset: mix(t, p.offset, q.offset),
                slope: mix(t, p.slope, q.slope),
            }
        }),
        dpc: near.dpc,
    }
}
