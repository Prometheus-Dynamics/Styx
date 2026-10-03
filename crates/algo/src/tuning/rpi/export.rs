//! Writing a [`Tuning`] as a Raspberry Pi tuning file (version 2), the inverse of the import:
//! for libcamera's Raspberry Pi IPA, and for tools that read that format.
//!
//! Every section the import reads is written back with the same key names and units (16-bit
//! levels, microseconds), in the order Raspberry Pi files use; values the Raspberry Pi IPA reads
//! as integers are rounded. Styx-only settings (AWB `softness` and `hysteresis`, AGC
//! `full_step`, black levels by gain, metering grids, AF `frame_exact`) have no Raspberry Pi key and are left
//! out, so importing the result gives Styx's defaults for them. Metering modes without tuned
//! weights get the built-in weights on the PiSP's 15×15 grid. The description has no place in
//! the format either.

use super::{FULL, alsc_grid};
use crate::algos::agc::metering;
use crate::pwl::Pwl;
use crate::tuning::*;

/// A JSON value with ordered object keys.
enum J {
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

fn num(v: f64) -> J {
    J::Num(v)
}

fn int(v: f64) -> J {
    J::Num(v.round())
}

fn flag(v: bool) -> J {
    J::Num(if v { 1.0 } else { 0.0 })
}

fn nums(v: &[f64]) -> J {
    J::Arr(v.iter().copied().map(J::Num).collect())
}

fn obj(fields: Vec<(&str, J)>) -> J {
    J::Obj(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

/// A piecewise linear function as the flat `[x0, y0, x1, y1, ...]` list.
fn pwl(p: &Pwl, sx: f64, sy: f64) -> J {
    nums(
        &p.points()
            .iter()
            .flat_map(|&(x, y)| [x * sx, y * sy])
            .collect::<Vec<_>>(),
    )
}

/// Map entries with `first` leading (the Raspberry Pi files' default is the first key).
fn default_first<'a, T>(
    map: impl IntoIterator<Item = (&'a String, T)>,
    first: &str,
) -> Vec<(&'a String, T)> {
    let mut v: Vec<_> = map.into_iter().collect();
    v.sort_by_key(|(k, _)| k.as_str() != first);
    v
}

fn write(v: &J, indent: usize, out: &mut String) {
    let pad = |n: usize| "    ".repeat(n);
    match v {
        J::Num(x) if x.is_finite() => out.push_str(&format!("{x}")),
        J::Num(_) => out.push('0'),
        J::Str(s) => {
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        J::Arr(items) if items.iter().all(|i| matches!(i, J::Num(_))) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write(item, indent, out);
            }
            out.push(']');
        }
        J::Arr(items) => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                out.push_str(&pad(indent + 1));
                write(item, indent + 1, out);
                out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
            }
            out.push_str(&pad(indent));
            out.push(']');
        }
        J::Obj(fields) => {
            out.push_str("{\n");
            for (i, (k, item)) in fields.iter().enumerate() {
                out.push_str(&pad(indent + 1));
                write(&J::Str(k.clone()), indent + 1, out);
                out.push_str(": ");
                write(item, indent + 1, out);
                out.push_str(if i + 1 < fields.len() { ",\n" } else { "\n" });
            }
            out.push_str(&pad(indent));
            out.push('}');
        }
    }
}

fn black_level(b: &BlackLevelTuning) -> J {
    obj(vec![
        ("black_level", int(b.g * FULL)),
        ("black_level_r", int(b.r * FULL)),
        ("black_level_g", int(b.g * FULL)),
        ("black_level_b", int(b.b * FULL)),
    ])
}

fn lux(l: &LuxTuning) -> J {
    obj(vec![
        ("reference_shutter_speed", num(l.reference_exposure_us)),
        ("reference_gain", num(l.reference_gain)),
        ("reference_aperture", num(l.reference_aperture)),
        ("reference_lux", num(l.reference_lux)),
        ("reference_Y", num(l.reference_y * FULL)),
    ])
}

fn agc(a: &AgcTuning) -> J {
    let mut names: Vec<&String> = a.metering_modes.keys().collect();
    let default_mm = a.default_metering_mode.clone();
    if !a.metering_modes.contains_key(&default_mm) {
        names.push(&default_mm);
    }
    let metering = default_first(names.into_iter().map(|k| (k, ())), &default_mm)
        .into_iter()
        .map(|(k, ())| {
            let tuned = a.metering_modes.get(k).filter(|m| m.grid.is_none());
            let w = match tuned {
                Some(m) => m.weights.clone(),
                None => metering::weights_for(k, a.metering_modes.get(k), 15, 15),
            };
            (k.clone(), obj(vec![("weights", nums(&w))]))
        })
        .collect();
    let exposure = default_first(&a.exposure_modes, &a.default_exposure_mode)
        .into_iter()
        .map(|(k, p)| {
            (
                k.clone(),
                obj(vec![
                    ("shutter", nums(&p.exposure_us)),
                    ("gain", nums(&p.gain)),
                ]),
            )
        })
        .collect();
    let constraints = default_first(&a.constraint_modes, &a.default_constraint_mode)
        .into_iter()
        .map(|(k, cs)| {
            let list = cs
                .iter()
                .map(|c| {
                    let bound = match c.bound {
                        Bound::Lower => "LOWER",
                        Bound::Upper => "UPPER",
                    };
                    obj(vec![
                        ("bound", J::Str(bound.into())),
                        ("q_lo", num(c.q_lo)),
                        ("q_hi", num(c.q_hi)),
                        ("y_target", pwl(&c.y_target, 1.0, 1.0)),
                    ])
                })
                .collect();
            (k.clone(), J::Arr(list))
        })
        .collect();
    obj(vec![
        ("metering_modes", J::Obj(metering)),
        ("exposure_modes", J::Obj(exposure)),
        ("constraint_modes", J::Obj(constraints)),
        ("y_target", pwl(&a.y_target, 1.0, 1.0)),
        ("speed", num(a.speed)),
        ("startup_frames", int(a.startup_frames.into())),
        ("convergence_frames", int(a.convergence_frames.into())),
        ("fast_reduce_threshold", num(a.fast_reduce_threshold)),
        ("base_ev", num(a.base_ev)),
        ("default_exposure_time", num(a.default_exposure_us)),
        ("default_analogue_gain", num(a.default_analogue_gain)),
        ("stable_region", num(a.stable_region)),
        ("desaturate", flag(a.desaturate)),
        ("max_digital_gain", num(a.max_digital_gain)),
    ])
}

fn awb(a: &AwbTuning) -> J {
    let mut f = vec![("bayes", flag(a.bayes))];
    if !a.ct_curve.is_empty() {
        f.push(("ct_curve", nums(&a.ct_curve.concat())));
    }
    if !a.priors.is_empty() {
        let priors = a
            .priors
            .iter()
            .map(|p| {
                obj(vec![
                    ("lux", num(p.lux)),
                    ("prior", pwl(&p.prior, 1.0, 1.0)),
                ])
            })
            .collect();
        f.push(("priors", J::Arr(priors)));
    }
    if !a.modes.is_empty() {
        let modes = default_first(&a.modes, &a.default_mode)
            .into_iter()
            .map(|(k, m)| (k.clone(), obj(vec![("lo", num(m.lo)), ("hi", num(m.hi))])))
            .collect();
        f.push(("modes", J::Obj(modes)));
    }
    f.extend([
        ("frame_period", int(a.frame_period.into())),
        ("startup_frames", int(a.startup_frames.into())),
        ("convergence_frames", int(a.convergence_frames.into())),
        ("speed", num(a.speed)),
        ("min_pixels", num(a.min_pixels)),
        ("min_G", num(a.min_g * FULL)),
        ("min_regions", int(a.min_regions.into())),
        ("coarse_step", num(a.coarse_step)),
        ("whitepoint_r", num(a.whitepoint_r)),
        ("whitepoint_b", num(a.whitepoint_b)),
        ("bias_proportion", num(a.bias_proportion)),
        ("bias_ct", num(a.bias_ct)),
        ("delta_limit", num(a.delta_limit)),
        ("transverse_pos", num(a.transverse_pos)),
        ("transverse_neg", num(a.transverse_neg)),
        ("sensitivity_r", num(a.sensitivity_r)),
        ("sensitivity_b", num(a.sensitivity_b)),
    ]);
    obj(f)
}

fn alsc(a: &AlscTuning) -> J {
    let cals = |list: &[AlscCalibration]| {
        J::Arr(
            list.iter()
                .map(|c| obj(vec![("ct", num(c.ct)), ("table", nums(&c.table))]))
                .collect(),
        )
    };
    let mut f = vec![
        ("omega", num(a.omega)),
        ("luminance_strength", num(a.luminance_strength)),
        ("calibrations_Cr", cals(&a.calibrations_cr)),
        ("calibrations_Cb", cals(&a.calibrations_cb)),
    ];
    if !a.luminance_lut.is_empty() {
        f.push(("luminance_lut", nums(&a.luminance_lut)));
    }
    if let Some(c) = a.corner_strength {
        f.push(("corner_strength", num(c)));
    }
    if let Some(n) = a.n_iter {
        f.push(("n_iter", int(n.into())));
    }
    f.extend([
        ("asymmetry", num(a.asymmetry)),
        ("default_ct", num(a.default_ct)),
        ("frame_period", int(a.frame_period.into())),
        ("startup_frames", int(a.startup_frames.into())),
        ("speed", num(a.speed)),
        ("sigma_Cr", num(a.sigma_cr)),
        ("sigma_Cb", num(a.sigma_cb)),
        ("min_count", num(a.min_count)),
        ("min_G", num(a.min_g * FULL)),
        ("threshold", num(a.threshold)),
        ("lambda_bound", num(a.lambda_bound)),
    ]);
    obj(f)
}

fn ccm(c: &CcmTuning) -> J {
    let ccms = c
        .ccms
        .iter()
        .map(|m| obj(vec![("ct", num(m.ct)), ("ccm", nums(&m.ccm))]))
        .collect();
    let mut f = vec![("ccms", J::Arr(ccms))];
    if let Some(s) = &c.saturation {
        f.push(("saturation", pwl(s, 1.0, 1.0)));
    }
    obj(f)
}

fn contrast(c: &ContrastTuning) -> J {
    obj(vec![
        ("ce_enable", flag(c.ce_enable)),
        ("lo_histogram", num(c.lo_histogram)),
        ("lo_level", num(c.lo_level)),
        ("lo_max", num(c.lo_max * FULL)),
        ("hi_histogram", num(c.hi_histogram)),
        ("hi_level", num(c.hi_level)),
        ("hi_max", num(c.hi_max * FULL)),
        ("gamma_curve", pwl(&c.gamma_curve, 65535.0, 65535.0)),
    ])
}

fn af(a: &crate::algos::af::AfTuning) -> J {
    use crate::algos::af::{AfRangeTuning, AfSpeedTuning};
    let range = |r: &AfRangeTuning| {
        obj(vec![
            ("min", num(r.min)),
            ("max", num(r.max)),
            ("default", num(r.default)),
        ])
    };
    let speed = |s: &AfSpeedTuning| {
        obj(vec![
            ("step_coarse", num(s.step_coarse)),
            ("step_fine", num(s.step_fine)),
            ("contrast_ratio", num(s.contrast_ratio)),
            ("retrigger_ratio", num(s.retrigger_ratio)),
            ("retrigger_delay", int(f64::from(s.retrigger_delay))),
            ("pdaf_gain", num(s.pdaf_gain)),
            ("pdaf_squelch", num(s.pdaf_squelch)),
            ("max_slew", num(s.max_slew)),
            ("pdaf_frames", int(f64::from(s.pdaf_frames))),
            ("dropout_frames", int(f64::from(s.dropout_frames))),
            ("step_frames", int(f64::from(s.step_frames))),
        ])
    };
    let mut ranges = vec![("normal", range(&a.ranges.normal))];
    if let Some(m) = &a.ranges.r#macro {
        ranges.push(("macro", range(m)));
    }
    if let Some(f) = &a.ranges.full {
        ranges.push(("full", range(f)));
    }
    let mut speeds = vec![("normal", speed(&a.speeds.normal))];
    if let Some(f) = &a.speeds.fast {
        speeds.push(("fast", speed(f)));
    }
    let mut f = vec![
        ("ranges", obj(ranges)),
        ("speeds", obj(speeds)),
        ("conf_epsilon", num(a.conf_epsilon)),
        ("conf_thresh", num(a.conf_thresh)),
        ("conf_clip", num(a.conf_clip)),
        ("skip_frames", int(f64::from(a.skip_frames))),
        ("check_for_ir", flag(a.check_for_ir)),
    ];
    if !a.map.is_empty() {
        f.push(("map", pwl(&a.map, 1.0, 1.0)));
    }
    obj(f)
}

/// `rpi.noise`, `rpi.geq`, `rpi.denoise`, `rpi.dpc` (in that order) and `rpi.sharpen` (last).
fn denoise(d: &DenoiseTuning) -> (Vec<(&'static str, J)>, Option<J>) {
    let mut out = vec![
        ("rpi.dpc", obj(vec![("strength", int(d.dpc.into()))])),
        (
            "rpi.noise",
            obj(vec![
                ("reference_constant", num(d.noise.reference_constant)),
                ("reference_slope", num(d.noise.reference_slope)),
            ]),
        ),
    ];
    if let Some(g) = &d.geq {
        let mut f = vec![("offset", int(g.offset)), ("slope", num(g.slope))];
        if let Some(s) = &g.strength {
            f.push(("strength", pwl(s, 1.0, 1.0)));
        }
        out.push(("rpi.geq", obj(f)));
    }
    let mut modes = Vec::new();
    if let Some(s) = &d.sdn {
        modes.push((
            "sdn",
            obj(vec![
                ("deviation", num(s.deviation)),
                ("strength", num(s.strength)),
                ("deviation2", num(s.deviation2)),
                ("deviation_no_tdn", num(s.deviation_no_tdn)),
                ("strength_no_tdn", num(s.strength_no_tdn)),
                ("backoff", num(s.backoff)),
            ]),
        ));
    }
    if let Some(c) = &d.cdn {
        let mut f = vec![
            ("deviation", num(c.deviation)),
            ("strength", num(c.strength)),
        ];
        if let Some(w) = c.deviation_with_tdn {
            f.push(("deviation_with_tdn", num(w)));
        }
        modes.push(("cdn", obj(f)));
    }
    if let Some(t) = &d.tdn {
        modes.push((
            "tdn",
            obj(vec![
                ("deviation", num(t.deviation)),
                ("threshold", num(t.threshold)),
            ]),
        ));
    }
    if !modes.is_empty() {
        out.push(("rpi.denoise", obj(vec![("normal", obj(modes))])));
    }
    let sharpen = d.sharpen.map(|s| {
        obj(vec![
            ("threshold", num(s.threshold)),
            ("strength", num(s.strength)),
            ("limit", num(s.limit)),
        ])
    });
    (out, sharpen)
}

/// The tuning as a Raspberry Pi tuning file. `target` is `pisp` or `bcm2835`; `None` picks
/// from the lens shading grid (16×12 tables are VC4's).
pub(in crate::tuning) fn export(t: &Tuning, target: Option<&str>) -> String {
    let vc4 = t.alsc.as_ref().map(|a| a.grid) == alsc_grid(192);
    let target = target.unwrap_or(if vc4 { "bcm2835" } else { "pisp" });
    let mut algos: Vec<(&str, J)> = Vec::new();
    if let Some(b) = &t.black_level {
        algos.push(("rpi.black_level", black_level(b)));
    }
    if let Some(l) = &t.lux {
        algos.push(("rpi.lux", lux(l)));
    }
    let (detail, sharpen) = t.denoise.as_ref().map(denoise).unwrap_or_default();
    algos.extend(detail);
    if let Some(a) = &t.awb {
        algos.push(("rpi.awb", awb(a)));
    }
    if let Some(a) = &t.agc {
        algos.push(("rpi.agc", agc(a)));
    }
    if let Some(a) = &t.alsc {
        algos.push(("rpi.alsc", alsc(a)));
    }
    if let Some(c) = &t.contrast {
        algos.push(("rpi.contrast", contrast(c)));
    }
    if let Some(c) = &t.ccm {
        algos.push(("rpi.ccm", ccm(c)));
    }
    if let Some(a) = &t.af {
        algos.push(("rpi.af", af(a)));
    }
    if let Some(s) = sharpen {
        algos.push(("rpi.sharpen", s));
    }
    let mut top = vec![("version", num(2.0)), ("target", J::Str(target.into()))];
    top.push((
        "algorithms",
        J::Arr(
            algos
                .into_iter()
                .map(|(k, v)| J::Obj(vec![(k.to_owned(), v)]))
                .collect(),
        ),
    ));
    let mut out = String::new();
    write(&obj(top), 0, &mut out);
    out.push('\n');
    out
}
