//! Naming what differs between two settings.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use styx_aaa::{
    CdnParams, DenoiseParams, GeqParams, LensShading, SdnParams, SharpenParams, TdnParams, ZoneGrid,
};

use crate::IspSettings;

impl IspSettings {
    /// The fields that differ between `self` and `other`, by name; empty when they are equal.
    ///
    /// A scalar is named by its field (`black_level`). An element of a fixed array or of a list
    /// is named by its index (`wb[1]`, `ccm[4]`, `flicker_bands`, `outputs[0].resolution`). A
    /// block that exists on both sides is named by its fields (`denoise.sdn.strength`,
    /// `lens_shading.r`, `histogram_weights.zones`). An option that is set on one side only is
    /// named whole (`sharpen`). Equal settings give an empty list, and `self == other` exactly
    /// when this list is empty.
    pub fn changed_fields(&self, other: &Self) -> Vec<String> {
        let (a, b) = (self, other);
        let mut out = Vec::new();
        mark(&mut out, "from_frame", a.from_frame != b.from_frame);
        mark(&mut out, "black_level", a.black_level != b.black_level);
        option(
            &mut out,
            "black_channels",
            &a.black_channels,
            &b.black_channels,
            |out, n, x, y| {
                for (i, (p, q)) in x.iter().zip(y).enumerate() {
                    mark(out, &format!("{n}[{i}]"), p != q);
                }
            },
        );
        for (i, (p, q)) in a.wb.iter().zip(b.wb).enumerate() {
            mark(&mut out, &format!("wb[{i}]"), *p != q);
        }
        mark(&mut out, "digital_gain", a.digital_gain != b.digital_gain);
        mark(&mut out, "flicker", a.flicker != b.flicker);
        option(
            &mut out,
            "flicker_bands",
            &a.flicker_bands,
            &b.flicker_bands,
            |out, n, x, y| {
                if x.len() != y.len() {
                    mark(out, &format!("{n}.len"), true);
                } else {
                    for (i, (p, q)) in x.iter().zip(y).enumerate() {
                        mark(out, &format!("{n}[{i}]"), p != q);
                    }
                }
            },
        );
        for (i, (p, q)) in a.ccm.iter().zip(b.ccm).enumerate() {
            mark(&mut out, &format!("ccm[{i}]"), *p != q);
        }
        option(&mut out, "gamma", &a.gamma, &b.gamma, |out, n, x, y| {
            mark(out, n, x != y)
        });
        option(
            &mut out,
            "lens_shading",
            &a.lens_shading,
            &b.lens_shading,
            lens_shading_diff,
        );
        denoise_diff(&mut out, &a.denoise, &b.denoise);
        option(&mut out, "sharpen", &a.sharpen, &b.sharpen, sharpen_diff);
        option(
            &mut out,
            "histogram_weights",
            &a.histogram_weights,
            &b.histogram_weights,
            grid_diff,
        );
        outputs_diff(&mut out, &a.outputs, &b.outputs);
        out
    }
}

fn mark(out: &mut Vec<String>, name: &str, differs: bool) {
    if differs {
        out.push(String::from(name));
    }
}

/// Both sides set: `nested` names the fields that differ. One side set: the option is named.
fn option<T: PartialEq>(
    out: &mut Vec<String>,
    name: &str,
    a: &Option<T>,
    b: &Option<T>,
    nested: impl FnOnce(&mut Vec<String>, &str, &T, &T),
) {
    match (a, b) {
        (Some(x), Some(y)) => nested(out, name, x, y),
        _ => mark(out, name, a != b),
    }
}

/// Names the fields of a struct that differ: `$prefix.field`.
macro_rules! fields {
    ($out:expr, $prefix:expr, $a:expr, $b:expr, [$($f:ident),* $(,)?]) => {
        $( mark($out, &format!("{}.{}", $prefix, stringify!($f)), $a.$f != $b.$f); )*
    };
}

fn lens_shading_diff(out: &mut Vec<String>, n: &str, a: &LensShading, b: &LensShading) {
    fields!(out, n, a, b, [width, height]);
    // Tables by their gains, not by their generation tag.
    mark(out, &format!("{n}.r"), a.r != b.r);
    mark(out, &format!("{n}.g"), a.g != b.g);
    mark(out, &format!("{n}.b"), a.b != b.b);
}

fn denoise_diff(out: &mut Vec<String>, a: &DenoiseParams, b: &DenoiseParams) {
    let n = "denoise";
    fields!(out, n, a, b, [noise_constant, noise_slope, dpc]);
    option(
        out,
        "denoise.sdn",
        &a.sdn,
        &b.sdn,
        |out, n, x: &SdnParams, y: &SdnParams| {
            fields!(
                out,
                n,
                x,
                y,
                [
                    noise_constant,
                    noise_slope,
                    noise_constant2,
                    noise_slope2,
                    strength
                ]
            );
        },
    );
    option(
        out,
        "denoise.cdn",
        &a.cdn,
        &b.cdn,
        |out, n, x: &CdnParams, y: &CdnParams| {
            fields!(out, n, x, y, [threshold, strength]);
        },
    );
    option(
        out,
        "denoise.tdn",
        &a.tdn,
        &b.tdn,
        |out, n, x: &TdnParams, y: &TdnParams| {
            fields!(out, n, x, y, [noise_constant, noise_slope, threshold]);
        },
    );
    option(
        out,
        "denoise.geq",
        &a.geq,
        &b.geq,
        |out, n, x: &GeqParams, y: &GeqParams| {
            fields!(out, n, x, y, [offset, slope]);
        },
    );
}

fn sharpen_diff(out: &mut Vec<String>, n: &str, a: &SharpenParams, b: &SharpenParams) {
    fields!(out, n, a, b, [threshold, strength, limit]);
}

fn grid_diff(out: &mut Vec<String>, n: &str, a: &ZoneGrid<f64>, b: &ZoneGrid<f64>) {
    fields!(out, n, a, b, [width, height]);
    mark(out, &format!("{n}.zones"), a.zones != b.zones);
}

fn outputs_diff(
    out: &mut Vec<String>,
    a: &[styx_format::MediaFormat],
    b: &[styx_format::MediaFormat],
) {
    if a.len() != b.len() {
        mark(out, "outputs.len", true);
    }
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let n = format!("outputs[{i}]");
        fields!(out, n, x, y, [code, resolution, color, range]);
    }
}
