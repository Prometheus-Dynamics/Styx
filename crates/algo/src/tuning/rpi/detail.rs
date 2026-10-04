//! `rpi.noise`, `rpi.denoise` (its `normal` mode, or the flat form), `rpi.sdn` (VC4),
//! `rpi.geq`, `rpi.dpc` and `rpi.sharpen` into [`DenoiseTuning`].

use alloc::{format, string::String, vec::Vec};

use super::*;

/// Merge one of the sections into `d`; false if `s` is not one of them.
pub(super) fn merge(s: &Section, d: &mut DenoiseTuning, ig: &mut Vec<String>) -> Result<bool> {
    match s.name.as_str() {
        "rpi.noise" => {
            s.unknown(&["reference_constant", "reference_slope"], ig);
            d.noise = NoiseTuning {
                reference_constant: s.need("reference_constant")?,
                reference_slope: s.need("reference_slope")?,
            };
        }
        "rpi.denoise" => {
            let modes = s.v.as_object().unwrap_or(&[]);
            let normal = modes.iter().find(|(k, _)| k == "normal");
            let body = match normal {
                Some((_, v)) => {
                    for (k, _) in modes {
                        if k != "normal" && k != "comment" {
                            ig.push(format!("rpi.denoise.{k}"));
                        }
                    }
                    s.sub("normal", v)
                }
                None => s.sub("", s.v),
            };
            denoise(&body, d, ig)?;
        }
        "rpi.sdn" => {
            s.unknown(&["deviation", "strength"], ig);
            let base = SdnTuning::default();
            let deviation = s.num_or("deviation", base.deviation)?;
            let strength = s.num_or("strength", base.strength)?;
            d.sdn = Some(SdnTuning {
                deviation,
                strength,
                deviation2: deviation,
                deviation_no_tdn: deviation,
                strength_no_tdn: strength,
                backoff: base.backoff,
            });
        }
        "rpi.geq" => {
            s.unknown(&["offset", "slope", "strength"], ig);
            d.geq = Some(GeqTuning {
                offset: s.need("offset")?,
                slope: s.need("slope")?,
                strength: match s.get("strength") {
                    Some(v) => Some(s.pwl(v, "strength")?),
                    None => None,
                },
            });
        }
        "rpi.dpc" => {
            s.unknown(&["strength"], ig);
            d.dpc = s.num_or("strength", 1.0)?.clamp(0.0, 2.0) as u8;
        }
        "rpi.sharpen" => {
            s.unknown(&["threshold", "strength", "limit"], ig);
            d.sharpen = Some(SharpenTuning {
                threshold: s.num_or("threshold", 1.0)?,
                strength: s.num_or("strength", 1.0)?,
                limit: s.num_or("limit", 1.0)?,
            });
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// One denoise configuration: `sdn`, `cdn` and `tdn`, defaulted as the Raspberry Pi IPA does
/// (without `tdn`, `sdn` and `cdn` keep their "no TDN" values throughout).
fn denoise(s: &Section, d: &mut DenoiseTuning, ig: &mut Vec<String>) -> Result<()> {
    s.unknown(&["sdn", "cdn", "tdn"], ig);
    if let Some(v) = s.get("sdn") {
        let n = s.sub("sdn", v);
        n.unknown(
            &[
                "deviation",
                "strength",
                "deviation2",
                "deviation_no_tdn",
                "strength_no_tdn",
                "backoff",
            ],
            ig,
        );
        let base = SdnTuning::default();
        let deviation = n.num_or("deviation", base.deviation)?;
        let strength = n.num_or("strength", base.strength)?;
        d.sdn = Some(SdnTuning {
            deviation,
            strength,
            deviation2: n.num_or("deviation2", deviation)?,
            deviation_no_tdn: n.num_or("deviation_no_tdn", deviation)?,
            strength_no_tdn: n.num_or("strength_no_tdn", strength)?,
            backoff: n.num_or("backoff", base.backoff)?,
        });
    }
    if let Some(v) = s.get("cdn") {
        let n = s.sub("cdn", v);
        n.unknown(
            &[
                "deviation",
                "deviation_no_tdn",
                "deviation_with_tdn",
                "strength",
            ],
            ig,
        );
        let base = CdnTuning::default();
        let no_tdn = n.num_or("deviation", base.deviation)?;
        d.cdn = Some(CdnTuning {
            deviation: n.num_or("deviation_no_tdn", no_tdn)?,
            deviation_with_tdn: n.num("deviation_with_tdn")?,
            strength: n.num_or("strength", base.strength)?,
        });
    }
    if let Some(v) = s.get("tdn") {
        let n = s.sub("tdn", v);
        n.unknown(&["deviation", "threshold"], ig);
        let base = TdnTuning::default();
        d.tdn = Some(TdnTuning {
            deviation: n.num_or("deviation", base.deviation)?,
            threshold: n.num_or("threshold", base.threshold)?,
        });
    } else {
        d.tdn = None;
    }
    Ok(())
}
