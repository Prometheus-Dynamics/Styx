//! Conversion of Raspberry Pi tuning files (version 2) into [`Tuning`].
//!
//! Only the algorithms implemented here are converted: `rpi.black_level`, `rpi.lux`,
//! `rpi.agc` (channel 0), `rpi.awb`, `rpi.alsc`,
//! `rpi.ccm`, `rpi.contrast`, `rpi.af`, and `rpi.noise`, `rpi.denoise`, `rpi.sdn`, `rpi.geq`, `rpi.dpc`,
//! `rpi.sharpen` (into [`DenoiseTuning`]). Everything else is listed in
//! [`RpiImport::ignored`].
//! 16-bit levels are normalised to 1.0 and times stay in microseconds.

use std::collections::BTreeMap;

use crate::error::{AlgoError, Result};
use crate::pwl::Pwl;

use super::json::Value;
use super::*;

mod af;
mod detail;

/// The result of converting a Raspberry Pi tuning file.
#[derive(Debug, Clone, PartialEq)]
pub struct RpiImport {
    /// The converted tuning.
    pub tuning: Tuning,
    /// The `target` field (`pisp` or `bcm2835`).
    pub target: Option<String>,
    /// Algorithms and keys that were not converted, e.g. `rpi.hdr` or `rpi.cac`.
    pub ignored: Vec<String>,
}

const FULL: f64 = 65536.0;

struct Section<'a> {
    name: String,
    v: &'a Value,
}

impl<'a> Section<'a> {
    fn err(&self, m: impl std::fmt::Display) -> AlgoError {
        AlgoError::Tuning(format!("{}: {m}", self.name))
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        self.v.get(key)
    }

    fn num(&self, key: &str) -> Result<Option<f64>> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => v
                .as_f64()
                .map(Some)
                .ok_or_else(|| self.err(format!("{key} must be a number"))),
        }
    }

    fn num_or(&self, key: &str, d: f64) -> Result<f64> {
        Ok(self.num(key)?.unwrap_or(d))
    }

    fn need(&self, key: &str) -> Result<f64> {
        self.num(key)?
            .ok_or_else(|| self.err(format!("{key} is required")))
    }

    fn nums(&self, v: &Value, what: &str) -> Result<Vec<f64>> {
        v.as_array()
            .ok_or_else(|| self.err(format!("{what} must be a list")))?
            .iter()
            .map(|x| {
                x.as_f64()
                    .ok_or_else(|| self.err(format!("{what} must hold numbers")))
            })
            .collect()
    }

    fn pwl(&self, v: &Value, what: &str) -> Result<Pwl> {
        match v.as_f64() {
            Some(y) => Ok(Pwl::constant(y)),
            None => {
                Pwl::from_flat(&self.nums(v, what)?).map_err(|e| self.err(format!("{what}: {e}")))
            }
        }
    }

    fn object(&self, key: &str) -> Result<&'a [(String, Value)]> {
        self.get(key)
            .and_then(Value::as_object)
            .ok_or_else(|| self.err(format!("{key} must be an object")))
    }

    fn sub(&self, key: &str, v: &'a Value) -> Section<'a> {
        Section {
            name: format!("{}.{key}", self.name),
            v,
        }
    }

    /// Keys present but not in `known`, as `name.key`.
    fn unknown(&self, known: &[&str], out: &mut Vec<String>) {
        for (k, _) in self.v.as_object().unwrap_or(&[]) {
            if !known.contains(&k.as_str()) && k != "comment" {
                out.push(format!("{}.{k}", self.name));
            }
        }
    }
}

pub(super) fn convert(doc: &Value) -> Result<RpiImport> {
    let version = doc.get("version").and_then(Value::as_f64);
    if version != Some(2.0) {
        return Err(AlgoError::tuning(format!(
            "only version 2 Raspberry Pi tunings are supported (found {version:?})"
        )));
    }
    let algorithms = doc
        .get("algorithms")
        .and_then(Value::as_array)
        .ok_or_else(|| AlgoError::tuning("algorithms must be a list"))?;
    let mut t = Tuning {
        description: Some("converted from a Raspberry Pi tuning".into()),
        ..Default::default()
    };
    let mut ignored = Vec::new();
    for entry in algorithms {
        let Some([(name, v)]) = entry.as_object() else {
            return Err(AlgoError::tuning("each algorithm must be a one-key object"));
        };
        let s = Section {
            name: name.clone(),
            v,
        };
        match name.as_str() {
            "rpi.black_level" => t.black_level = Some(black_level(&s, &mut ignored)?),
            "rpi.lux" => t.lux = Some(lux(&s, &mut ignored)?),
            "rpi.agc" => t.agc = Some(agc(&s, &mut ignored)?),
            "rpi.awb" => t.awb = Some(awb(&s, &mut ignored)?),
            "rpi.alsc" => t.alsc = Some(alsc(&s, &mut ignored)?),
            "rpi.ccm" => t.ccm = Some(ccm(&s, &mut ignored)?),
            "rpi.contrast" => t.contrast = Some(contrast(&s, &mut ignored)?),
            "rpi.af" => t.af = Some(af::convert(&s, &mut ignored)?),
            _ => {
                let mut d = t.denoise.clone().unwrap_or_default();
                if detail::merge(&s, &mut d, &mut ignored)? {
                    t.denoise = Some(d);
                } else {
                    ignored.push(name.clone());
                }
            }
        }
    }
    Ok(RpiImport {
        tuning: t,
        target: doc.get("target").and_then(Value::as_str).map(str::to_owned),
        ignored,
    })
}

fn black_level(s: &Section, ig: &mut Vec<String>) -> Result<BlackLevelTuning> {
    s.unknown(
        &[
            "black_level",
            "black_level_r",
            "black_level_g",
            "black_level_b",
        ],
        ig,
    );
    let all = s.num_or("black_level", 4096.0)?;
    Ok(BlackLevelTuning {
        r: s.num_or("black_level_r", all)? / FULL,
        g: s.num_or("black_level_g", all)? / FULL,
        b: s.num_or("black_level_b", all)? / FULL,
    })
}

fn lux(s: &Section, ig: &mut Vec<String>) -> Result<LuxTuning> {
    s.unknown(
        &[
            "reference_shutter_speed",
            "reference_gain",
            "reference_aperture",
            "reference_lux",
            "reference_Y",
        ],
        ig,
    );
    Ok(LuxTuning {
        reference_exposure_us: s.need("reference_shutter_speed")?,
        reference_gain: s.need("reference_gain")?,
        reference_aperture: s.num_or("reference_aperture", 1.0)?,
        reference_lux: s.need("reference_lux")?,
        reference_y: s.need("reference_Y")? / FULL,
    })
}

/// Entries of an ordered map section, and the first key (the default).
fn named<'a>(s: &Section<'a>, key: &str) -> Result<(Vec<(String, Section<'a>)>, String)> {
    let o = s.object(key)?;
    let first = o
        .first()
        .map(|(k, _)| k.clone())
        .ok_or_else(|| s.err(format!("{key} is empty")))?;
    Ok((
        o.iter()
            .map(|(k, v)| (k.clone(), s.sub(&format!("{key}.{k}"), v)))
            .collect(),
        first,
    ))
}

fn agc(s: &Section, ig: &mut Vec<String>) -> Result<AgcTuning> {
    // Newer files have "channels" (HDR); channel 0 is normal AGC.
    let owned;
    let s = match s.get("channels") {
        Some(ch) => {
            let list = ch
                .as_array()
                .filter(|l| !l.is_empty())
                .ok_or_else(|| s.err("channels must be a non-empty list"))?;
            for i in 1..list.len() {
                ig.push(format!("{}.channels[{i}]", s.name));
            }
            owned = s.sub("channels[0]", &list[0]);
            &owned
        }
        None => s,
    };
    s.unknown(
        &[
            "metering_modes",
            "exposure_modes",
            "constraint_modes",
            "channel_constraints",
            "y_target",
            "speed",
            "startup_frames",
            "convergence_frames",
            "fast_reduce_threshold",
            "base_ev",
            "default_exposure_time",
            "default_analogue_gain",
            "stable_region",
            "desaturate",
            "max_digital_gain",
        ],
        ig,
    );
    if s.get("channel_constraints").is_some() {
        ig.push(format!("{}.channel_constraints", s.name));
    }
    let d = AgcTuning::default();
    let (mm, default_metering_mode) = named(s, "metering_modes")?;
    let mut metering_modes = BTreeMap::new();
    for (k, m) in mm {
        let w = m.get("weights").ok_or_else(|| m.err("weights required"))?;
        metering_modes.insert(
            k,
            MeteringMode {
                weights: m.nums(w, "weights")?,
                grid: None,
            },
        );
    }
    let (em, default_exposure_mode) = named(s, "exposure_modes")?;
    let mut exposure_modes = BTreeMap::new();
    for (k, m) in em {
        let get = |key| m.get(key).ok_or_else(|| m.err(format!("{key} required")));
        exposure_modes.insert(
            k,
            ExposureProfile {
                exposure_us: m.nums(get("shutter")?, "shutter")?,
                gain: m.nums(get("gain")?, "gain")?,
            },
        );
    }
    let (cm, default_constraint_mode) = named(s, "constraint_modes")?;
    let mut constraint_modes = BTreeMap::new();
    for (k, m) in cm {
        let list = m.v.as_array().ok_or_else(|| m.err("must be a list"))?;
        let mut cs = Vec::new();
        for c in list {
            let c = m.sub("constraint", c);
            let bound = match c
                .get("bound")
                .and_then(Value::as_str)
                .map(str::to_ascii_uppercase)
                .as_deref()
            {
                Some("UPPER") => Bound::Upper,
                Some("LOWER") => Bound::Lower,
                _ => return Err(c.err("bound must be UPPER or LOWER")),
            };
            let y = c
                .get("y_target")
                .ok_or_else(|| c.err("y_target required"))?;
            cs.push(Constraint {
                bound,
                q_lo: c.need("q_lo")?,
                q_hi: c.need("q_hi")?,
                y_target: c.pwl(y, "y_target")?,
            });
        }
        constraint_modes.insert(k, cs);
    }
    let y = s
        .get("y_target")
        .ok_or_else(|| s.err("y_target required"))?;
    Ok(AgcTuning {
        metering_modes,
        default_metering_mode,
        exposure_modes,
        default_exposure_mode,
        constraint_modes,
        default_constraint_mode,
        y_target: s.pwl(y, "y_target")?,
        speed: s.num_or("speed", d.speed)?,
        // Not a Raspberry Pi key: Styx's model-based steps (see `AgcTuning::full_step`).
        full_step: d.full_step,
        startup_frames: s.num_or("startup_frames", d.startup_frames.into())? as u32,
        convergence_frames: s.num_or("convergence_frames", d.convergence_frames.into())? as u32,
        fast_reduce_threshold: s.num_or("fast_reduce_threshold", d.fast_reduce_threshold)?,
        base_ev: s.num_or("base_ev", d.base_ev)?,
        default_exposure_us: s.num_or("default_exposure_time", d.default_exposure_us)?,
        default_analogue_gain: s.num_or("default_analogue_gain", d.default_analogue_gain)?,
        stable_region: s.num_or("stable_region", d.stable_region)?,
        desaturate: s.num_or("desaturate", 1.0)? != 0.0,
        max_digital_gain: s.num_or("max_digital_gain", d.max_digital_gain)?,
    })
}

fn awb(s: &Section, ig: &mut Vec<String>) -> Result<AwbTuning> {
    s.unknown(
        &[
            "bayes",
            "ct_curve",
            "priors",
            "modes",
            "frame_period",
            "startup_frames",
            "convergence_frames",
            "speed",
            "min_pixels",
            "min_G",
            "min_regions",
            "coarse_step",
            "whitepoint_r",
            "whitepoint_b",
            "bias_proportion",
            "bias_ct",
            "delta_limit",
            "transverse_pos",
            "transverse_neg",
            "sensitivity_r",
            "sensitivity_b",
        ],
        ig,
    );
    let d = AwbTuning::default();
    let mut ct_curve = Vec::new();
    if let Some(v) = s.get("ct_curve") {
        let n = s.nums(v, "ct_curve")?;
        if n.len() % 3 != 0 || n.len() < 6 {
            return Err(s.err("ct_curve needs at least two ct, r, b triples"));
        }
        ct_curve = n.chunks(3).map(|c| [c[0], c[1], c[2]]).collect();
    }
    let mut priors = Vec::new();
    if let Some(v) = s.get("priors") {
        for p in v.as_array().ok_or_else(|| s.err("priors must be a list"))? {
            let p = s.sub("priors", p);
            let prior = p.get("prior").ok_or_else(|| p.err("prior required"))?;
            priors.push(AwbPrior {
                lux: p.need("lux")?,
                prior: p.pwl(prior, "prior")?,
            });
        }
    }
    let mut modes = BTreeMap::new();
    let mut default_mode = d.default_mode.clone();
    if s.get("modes").is_some() {
        let (list, first) = named(s, "modes")?;
        default_mode = first;
        for (k, m) in list {
            modes.insert(
                k,
                AwbMode {
                    lo: m.need("lo")?,
                    hi: m.need("hi")?,
                },
            );
        }
    }
    Ok(AwbTuning {
        bayes: s.num_or("bayes", 1.0)? != 0.0,
        ct_curve,
        priors,
        modes,
        default_mode,
        frame_period: s.num_or("frame_period", d.frame_period.into())? as u32,
        startup_frames: s.num_or("startup_frames", d.startup_frames.into())? as u32,
        convergence_frames: s.num_or("convergence_frames", d.convergence_frames.into())? as u32,
        speed: s.num_or("speed", d.speed)?,
        min_pixels: s.num_or("min_pixels", d.min_pixels)?,
        min_g: s.num_or("min_G", 32.0)? / FULL,
        min_regions: s.num_or("min_regions", d.min_regions.into())? as u32,
        coarse_step: s.num_or("coarse_step", d.coarse_step)?,
        whitepoint_r: s.num_or("whitepoint_r", 0.0)?,
        whitepoint_b: s.num_or("whitepoint_b", 0.0)?,
        bias_proportion: s.num_or("bias_proportion", 0.0)?,
        bias_ct: s.num_or("bias_ct", d.bias_ct)?,
        delta_limit: s.num_or("delta_limit", d.delta_limit)?,
        transverse_pos: s.num_or("transverse_pos", d.transverse_pos)?,
        transverse_neg: s.num_or("transverse_neg", d.transverse_neg)?,
        sensitivity_r: s.num_or("sensitivity_r", 1.0)?,
        sensitivity_b: s.num_or("sensitivity_b", 1.0)?,
        ..d
    })
}

/// Grid of a Raspberry Pi ALSC table from its size: 32×32 on PiSP, 16×12 on VC4.
fn alsc_grid(cells: usize) -> Option<(u32, u32)> {
    match cells {
        1024 => Some((32, 32)),
        192 => Some((16, 12)),
        _ => None,
    }
}

fn alsc(s: &Section, ig: &mut Vec<String>) -> Result<AlscTuning> {
    s.unknown(
        &[
            "calibrations_Cr",
            "calibrations_Cb",
            "luminance_lut",
            "corner_strength",
            "asymmetry",
            "luminance_strength",
            "default_ct",
            "frame_period",
            "startup_frames",
            "speed",
            "sigma",
            "sigma_Cr",
            "sigma_Cb",
            "min_count",
            "min_G",
            "omega",
            "n_iter",
            "threshold",
            "lambda_bound",
        ],
        ig,
    );
    let d = AlscTuning::default();
    let cals = |key: &str| -> Result<Vec<AlscCalibration>> {
        let Some(v) = s.get(key) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for c in v
            .as_array()
            .ok_or_else(|| s.err(format!("{key} must be a list")))?
        {
            let c = s.sub(key, c);
            let t = c.get("table").ok_or_else(|| c.err("table required"))?;
            out.push(AlscCalibration {
                ct: c.need("ct")?,
                table: c.nums(t, "table")?,
            });
        }
        Ok(out)
    };
    let (cr, cb) = (cals("calibrations_Cr")?, cals("calibrations_Cb")?);
    let lut = match s.get("luminance_lut") {
        Some(v) => s.nums(v, "luminance_lut")?,
        None => Vec::new(),
    };
    let cells = cr
        .first()
        .or(cb.first())
        .map(|c| c.table.len())
        .unwrap_or(lut.len());
    let grid = if cells == 0 {
        d.grid
    } else {
        alsc_grid(cells)
            .ok_or_else(|| s.err(format!("cannot infer the table grid from {cells} cells")))?
    };
    Ok(AlscTuning {
        grid,
        calibrations_cr: cr,
        calibrations_cb: cb,
        luminance_lut: lut,
        corner_strength: s.num("corner_strength")?,
        asymmetry: s.num_or("asymmetry", 1.0)?,
        luminance_strength: s.num_or("luminance_strength", 1.0)?,
        default_ct: s.num_or("default_ct", d.default_ct)?,
        frame_period: s.num_or("frame_period", f64::from(d.frame_period))? as u32,
        startup_frames: s.num_or("startup_frames", f64::from(d.startup_frames))? as u32,
        speed: s.num_or("speed", d.speed)?,
        sigma_cr: s.num_or("sigma_Cr", s.num_or("sigma", d.sigma_cr)?)?,
        sigma_cb: s.num_or("sigma_Cb", s.num_or("sigma", d.sigma_cb)?)?,
        min_count: s.num_or("min_count", d.min_count)?,
        min_g: s.num("min_G")?.map_or(d.min_g, |g| g / 65536.0),
        omega: s.num_or("omega", d.omega)?,
        n_iter: s.num("n_iter")?.map(|n| n as u32),
        threshold: s.num_or("threshold", d.threshold)?,
        lambda_bound: s.num_or("lambda_bound", d.lambda_bound)?,
    })
}

fn ccm(s: &Section, ig: &mut Vec<String>) -> Result<CcmTuning> {
    s.unknown(&["ccms", "saturation"], ig);
    let list = s
        .get("ccms")
        .and_then(Value::as_array)
        .ok_or_else(|| s.err("ccms must be a list"))?;
    let mut ccms = Vec::new();
    for c in list {
        let c = s.sub("ccms", c);
        let m = c.get("ccm").ok_or_else(|| c.err("ccm required"))?;
        let m = c.nums(m, "ccm")?;
        let ccm: [f64; 9] = m.try_into().map_err(|_| c.err("ccm needs 9 values"))?;
        ccms.push(CtCcm {
            ct: c.need("ct")?,
            ccm,
        });
    }
    let saturation = match s.get("saturation") {
        Some(v) => Some(s.pwl(v, "saturation")?),
        None => None,
    };
    Ok(CcmTuning { ccms, saturation })
}

fn contrast(s: &Section, ig: &mut Vec<String>) -> Result<ContrastTuning> {
    s.unknown(
        &[
            "ce_enable",
            "lo_histogram",
            "lo_level",
            "lo_max",
            "hi_histogram",
            "hi_level",
            "hi_max",
            "gamma_curve",
        ],
        ig,
    );
    let d = ContrastTuning::default();
    let g = s
        .get("gamma_curve")
        .ok_or_else(|| s.err("gamma_curve required"))?;
    let gamma = s.pwl(g, "gamma_curve")?;
    let gamma_curve = gamma
        .map_x(|x| x / 65535.0)
        .map_err(|e| s.err(e))?
        .map_y(|_, y| y / 65535.0);
    Ok(ContrastTuning {
        ce_enable: s.num_or("ce_enable", 1.0)? != 0.0,
        lo_histogram: s.num_or("lo_histogram", d.lo_histogram)?,
        lo_level: s.num_or("lo_level", d.lo_level)?,
        lo_max: s.num_or("lo_max", 500.0)? / FULL,
        hi_histogram: s.num_or("hi_histogram", d.hi_histogram)?,
        hi_level: s.num_or("hi_level", d.hi_level)?,
        hi_max: s.num_or("hi_max", 2000.0)? / FULL,
        gamma_curve,
    })
}
