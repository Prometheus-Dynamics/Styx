//! `rpi.af` into [`AfTuning`]: the same names and units (dioptres, frames, the sensor's PDAF
//! units, the dioptre → lens driver map).

use alloc::{format, string::String, vec::Vec};

#[cfg(not(feature = "std"))]
use crate::math::Float as _;

use super::*;

const RANGE_KEYS: [&str; 3] = ["min", "max", "default"];
const SPEED_KEYS: [&str; 11] = [
    "step_coarse",
    "step_fine",
    "contrast_ratio",
    "retrigger_ratio",
    "retrigger_delay",
    "pdaf_gain",
    "pdaf_squelch",
    "max_slew",
    "pdaf_frames",
    "dropout_frames",
    "step_frames",
];

fn count(s: &Section, key: &str, d: u32) -> Result<u32> {
    let v = s.num_or(key, f64::from(d))?;
    if !(v >= 0.0 && v.fract() == 0.0 && v <= f64::from(u32::MAX)) {
        return Err(s.err(format!("{key} must be a whole number")));
    }
    Ok(v as u32)
}

fn range(s: &Section, base: AfRangeTuning, ig: &mut Vec<String>) -> Result<AfRangeTuning> {
    s.unknown(&RANGE_KEYS, ig);
    Ok(AfRangeTuning {
        min: s.num_or("min", base.min)?,
        max: s.num_or("max", base.max)?,
        default: s.num_or("default", base.default)?,
    })
}

fn speed(s: &Section, b: AfSpeedTuning, ig: &mut Vec<String>) -> Result<AfSpeedTuning> {
    s.unknown(&SPEED_KEYS, ig);
    Ok(AfSpeedTuning {
        step_coarse: s.num_or("step_coarse", b.step_coarse)?,
        step_fine: s.num_or("step_fine", b.step_fine)?,
        contrast_ratio: s.num_or("contrast_ratio", b.contrast_ratio)?,
        retrigger_ratio: s.num_or("retrigger_ratio", b.retrigger_ratio)?,
        retrigger_delay: count(s, "retrigger_delay", b.retrigger_delay)?,
        pdaf_gain: s.num_or("pdaf_gain", b.pdaf_gain)?,
        pdaf_squelch: s.num_or("pdaf_squelch", b.pdaf_squelch)?,
        max_slew: s.num_or("max_slew", b.max_slew)?,
        pdaf_frames: count(s, "pdaf_frames", b.pdaf_frames)?,
        dropout_frames: count(s, "dropout_frames", b.dropout_frames)?,
        step_frames: count(s, "step_frames", b.step_frames)?,
    })
}

/// Converts `rpi.af`. As libcamera: `macro` starts from `normal`, `full` from their union,
/// `fast` from `normal`.
pub(super) fn convert(s: &Section, ig: &mut Vec<String>) -> Result<AfTuning> {
    s.unknown(
        &[
            "ranges",
            "speeds",
            "conf_epsilon",
            "conf_thresh",
            "conf_clip",
            "skip_frames",
            "check_for_ir",
            "map",
        ],
        ig,
    );
    let mut t = AfTuning::default();
    if let Some(v) = s.get("ranges") {
        let rr = s.sub("ranges", v);
        rr.unknown(&["normal", "macro", "full"], ig);
        let get = |k: &str| rr.get(k).map(|v| rr.sub(k, v));
        if let Some(n) = get("normal") {
            t.ranges.normal = range(&n, t.ranges.normal, ig)?;
        }
        if let Some(m) = get("macro") {
            t.ranges.r#macro = Some(range(&m, t.ranges.normal, ig)?);
        }
        if let Some(f) = get("full") {
            let base = t.ranges.get(crate::algos::af::AfRange::Full);
            t.ranges.full = Some(range(&f, base, ig)?);
        }
    }
    if let Some(v) = s.get("speeds") {
        let ss = s.sub("speeds", v);
        ss.unknown(&["normal", "fast"], ig);
        if let Some(v) = ss.get("normal") {
            t.speeds.normal = speed(&ss.sub("normal", v), t.speeds.normal, ig)?;
        }
        if let Some(v) = ss.get("fast") {
            t.speeds.fast = Some(speed(&ss.sub("fast", v), t.speeds.normal, ig)?);
        }
    }
    t.conf_epsilon = s.num_or("conf_epsilon", t.conf_epsilon)?;
    t.conf_thresh = s.num_or("conf_thresh", t.conf_thresh)?;
    t.conf_clip = s.num_or("conf_clip", t.conf_clip)?;
    t.skip_frames = count(s, "skip_frames", t.skip_frames)?;
    if let Some(v) = s.get("check_for_ir") {
        t.check_for_ir = v
            .as_f64()
            .map(|x| x != 0.0)
            .ok_or_else(|| s.err("check_for_ir must be a boolean"))?;
    }
    if let Some(v) = s.get("map") {
        t.map = s.pwl(v, "map")?;
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use crate::Tuning;
    use crate::algos::af::{AfRange, AfSpeed};

    #[test]
    fn imx708_af_section_converts() {
        // The `rpi.af` section of Raspberry Pi's imx708.json (BSD-2-Clause).
        let text = r#"{"version": 2.0, "algorithms": [{"rpi.af": {
            "ranges": {"normal": {"min": 0.0, "max": 12.0, "default": 1.0},
                       "macro": {"min": 3.0, "max": 15.0, "default": 4.0}},
            "speeds": {"normal": {"step_coarse": 1.0, "step_fine": 0.25,
                "contrast_ratio": 0.75, "retrigger_ratio": 0.8, "retrigger_delay": 10,
                "pdaf_gain": -0.016, "pdaf_squelch": 0.125, "max_slew": 1.5,
                "pdaf_frames": 20, "dropout_frames": 6, "step_frames": 5},
                "fast": {"step_coarse": 1.25, "step_fine": 0.0}},
            "conf_epsilon": 8, "conf_thresh": 16, "conf_clip": 512, "skip_frames": 5,
            "check_for_ir": false, "map": [0.0, 445, 15.0, 925], "wobble": 1}}]}"#;
        let import = Tuning::from_rpi_json_str(text).unwrap();
        let af = import.tuning.af.unwrap();
        assert_eq!(af.ranges.get(AfRange::Macro).default, 4.0);
        let full = af.ranges.get(AfRange::Full);
        assert_eq!((full.min, full.max, full.default), (0.0, 15.0, 1.0));
        let fast = af.speeds.get(AfSpeed::Fast);
        // Fast starts from normal.
        assert_eq!(
            (fast.step_coarse, fast.step_fine, fast.max_slew),
            (1.25, 0.0, 1.5)
        );
        assert_eq!(af.speeds.normal.pdaf_gain, -0.016);
        assert_eq!(af.map.eval(15.0), 925.0);
        assert_eq!(import.ignored, ["rpi.af.wobble"]);
        // And back through our TOML.
        let t = Tuning {
            af: Some(af.clone()),
            ..Tuning::default()
        };
        let back = Tuning::from_toml_str(&t.to_toml_string().unwrap()).unwrap();
        assert_eq!(back.af.unwrap(), af);
    }
}
