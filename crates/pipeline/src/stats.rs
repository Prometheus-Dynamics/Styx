//! ISP statistics → [`styx_algo::Statistics`] (normalised to full scale 1.0, black level
//! removed, before white balance).

use alloc::vec;
use alloc::vec::Vec;
#[cfg(not(feature = "std"))]
use styx_core::math::Float as _;

use styx_algo::{ColourZone, Histogram, LumaZone, Statistics, ZoneGrid};
use styx_pisp::stats::Statistics as PispStatistics;
use styx_pisp::uapi::{AWB_STATS_SIZE, CDAF_STATS_SIZE, RawStatistics};
use styx_softisp::IspStats;

/// Full scale of the PiSP statistics: sums are of 16-bit samples after the statistics black
/// level (`BLC`).
const PISP_SCALE: f64 = 65536.0;
/// Full scale of the software ISP's working samples (12 bits).
const SOFT_SCALE: f64 = 4095.0;

/// PiSP front end statistics: the 32x32 AWB zones become the colour zones, the 1024-bin
/// Y histogram (weighted by the RGB-to-Y gains in the front end config) the histogram, the
/// 8x8 CDAF figures of merit the focus values. The front end applies neither white balance
/// nor (here) lens shading before the statistics.
pub fn from_pisp(s: &PispStatistics) -> Statistics {
    let side = AWB_STATS_SIZE as u32;
    let zones = s
        .awb_zones
        .iter()
        .map(|z| ColourZone {
            r: f64::from(z.r_sum) / PISP_SCALE,
            g: f64::from(z.g_sum) / PISP_SCALE,
            b: f64::from(z.b_sum) / PISP_SCALE,
            counted: z.counted,
        })
        .collect();
    let focus_side = CDAF_STATS_SIZE as u32;
    Statistics {
        colour: ZoneGrid {
            width: side,
            height: side,
            zones,
        },
        luma: None,
        histogram: Histogram::from(
            s.histogram
                .iter()
                .map(|&c| u64::from(c))
                .collect::<Vec<_>>(),
        ),
        focus: Some(ZoneGrid {
            width: focus_side,
            height: focus_side,
            zones: s.focus.iter().map(|&f| f as f64).collect(),
        }),
        pdaf: None,
        before_wb: true,
        before_lsc: true,
    }
}

/// [`from_pisp`] straight from the raw buffer layout into `out`, reusing its buffers (no
/// allocation once `out` has held PiSP statistics).
pub fn from_pisp_raw(raw: &RawStatistics, out: &mut Statistics) {
    let side = AWB_STATS_SIZE as u32;
    out.colour.width = side;
    out.colour.height = side;
    out.colour.zones.clear();
    out.colour
        .zones
        .extend(raw.awb.zones.iter().map(|z| ColourZone {
            r: f64::from(z.r_sum) / PISP_SCALE,
            g: f64::from(z.g_sum) / PISP_SCALE,
            b: f64::from(z.b_sum) / PISP_SCALE,
            counted: z.counted,
        }));
    out.luma = None;
    let mut bins: Vec<u64> = core::mem::take(&mut out.histogram).into();
    bins.clear();
    bins.extend(raw.agc.histogram.iter().map(|&c| u64::from(c)));
    out.histogram = Histogram::from(bins);
    let focus_side = CDAF_STATS_SIZE as u32;
    let focus = out.focus.get_or_insert_with(Default::default);
    focus.width = focus_side;
    focus.height = focus_side;
    focus.zones.clear();
    focus.zones.extend(raw.cdaf.foms.iter().map(|&f| f as f64));
    out.before_wb = true;
    out.before_lsc = true;
}

/// Software ISP statistics. Its sums include the white balance and digital gains it applied
/// (divided out here), the lens shading (`before_lsc` is false) and its rescaling of the range
/// above black to full scale (undone with `black_level`, the normalised black level, so values
/// match the PiSP's convention of black-subtracted samples). Luma zones keep the white balance
/// (as the PiSP's Y weights do) but not the digital gain; the histogram is resampled the
/// same way.
pub fn from_softisp(s: &IspStats, black_level: f64, with_lens_shading: bool) -> Statistics {
    let range = 1.0 - black_level.clamp(0.0, 0.99);
    let g = s.gains.map(|g| f64::from(g).max(1e-6));
    let zones: Vec<ColourZone> = s
        .zones
        .iter()
        .map(|z| ColourZone {
            r: z.r_sum as f64 / SOFT_SCALE / g[0] * range,
            g: z.g_sum as f64 / SOFT_SCALE / g[1] * range,
            b: z.b_sum as f64 / SOFT_SCALE / g[2] * range,
            counted: z.count,
        })
        .collect();
    let n = (s.zones_x * s.zones_y).max(1);
    let per_zone = s.samples / n;
    let luma = s
        .zones
        .iter()
        .map(|z| LumaZone {
            y: f64::from(z.luma) / g[1] * range * f64::from(per_zone),
            counted: per_zone,
        })
        .collect();
    Statistics {
        colour: ZoneGrid {
            width: s.zones_x,
            height: s.zones_y,
            zones,
        },
        luma: Some(ZoneGrid {
            width: s.zones_x,
            height: s.zones_y,
            zones: luma,
        }),
        histogram: Histogram::from(rescale_histogram(&s.histogram, range / g[1])),
        // Gradient energy without the gains (squared) and on the black-subtracted scale, as
        // the colour zones.
        focus: (s.focus.len() == s.zones.len() && !s.focus.is_empty()).then(|| ZoneGrid {
            width: s.zones_x,
            height: s.zones_y,
            zones: s.focus.iter().map(|f| f * (range / g[1]).powi(2)).collect(),
        }),
        pdaf: None,
        before_wb: true,
        before_lsc: !with_lens_shading,
    }
}

/// A histogram of values `v` turned into one of `v × factor` over the same bins (counts
/// moved to the bin their centre lands in, clipped into the last bin).
pub fn rescale_histogram(bins: &[u32], factor: f64) -> Vec<u64> {
    let n = bins.len();
    let mut out = vec![0u64; n];
    if n == 0 {
        return out;
    }
    for (i, &c) in bins.iter().enumerate() {
        let pos = ((i as f64 + 0.5) * factor).floor();
        let j = if pos.is_finite() && pos > 0.0 {
            (pos as usize).min(n - 1)
        } else {
            0
        };
        out[j] += u64::from(c);
    }
    out
}

/// Mean of a normalised luma histogram, 0..1.
pub fn histogram_mean(h: &Histogram) -> f64 {
    let total = h.total();
    if total == 0 {
        return 0.0;
    }
    let n = h.len() as f64;
    let sum: f64 = h
        .bins()
        .iter()
        .enumerate()
        .map(|(i, &c)| (i as f64 + 0.5) * c as f64)
        .sum();
    sum / total as f64 / n
}

#[cfg(test)]
mod tests {
    use styx_pisp::uapi::RawStatistics;
    use styx_softisp::ZoneStats;

    use super::*;

    #[test]
    fn pisp_sums_are_normalised_to_16_bits() {
        let mut raw: RawStatistics = bytemuck_zeroed();
        raw.awb.zones[0].r_sum = 65536 * 3;
        raw.awb.zones[0].g_sum = 65536 * 6;
        raw.awb.zones[0].b_sum = 65536;
        raw.awb.zones[0].counted = 12;
        raw.agc.histogram[512] = 100;
        raw.cdaf.foms[5] = 9;
        let s = from_pisp(&PispStatistics::from_raw(&raw));
        assert_eq!((s.colour.width, s.colour.height), (32, 32));
        assert_eq!(s.colour.zones[0].mean(), (0.25, 0.5, 1.0 / 12.0));
        assert_eq!(s.histogram.len(), 1024);
        assert!((histogram_mean(&s.histogram) - 512.5 / 1024.0).abs() < 1e-12);
        assert_eq!(s.focus.as_ref().unwrap().zones[5], 9.0);
        assert!(s.before_wb && s.luma.is_none());
        // The in-place conversion gives the same, into reused buffers.
        let mut out = from_softisp_like_garbage();
        from_pisp_raw(&raw, &mut out);
        assert_eq!(out, s);
        from_pisp_raw(&raw, &mut out);
        assert_eq!(out, s);
    }

    /// Statistics holding something else (as a reused buffer would).
    fn from_softisp_like_garbage() -> Statistics {
        Statistics {
            colour: ZoneGrid {
                width: 2,
                height: 1,
                zones: vec![ColourZone::default(); 2],
            },
            luma: Some(ZoneGrid::default()),
            histogram: Histogram::from(vec![1, 2, 3]),
            focus: None,
            pdaf: None,
            before_wb: false,
            before_lsc: false,
        }
    }

    fn bytemuck_zeroed() -> RawStatistics {
        bytemuck::Zeroable::zeroed()
    }

    #[test]
    fn softisp_gains_and_range_are_divided_out() {
        let bl = 64.0 / 1023.0;
        let st = IspStats {
            zones_x: 2,
            zones_y: 1,
            zones: vec![
                ZoneStats {
                    r_sum: 4095 * 2 * 10,
                    g_sum: 4095 * 10,
                    b_sum: 4095 * 4 * 10,
                    count: 10,
                    luma: 0.5,
                },
                ZoneStats::default(),
            ],
            histogram: vec![0, 0, 10, 10],
            samples: 20,
            gains: [2.0, 1.0, 4.0],
            focus: Vec::new(),
        };
        let s = from_softisp(&st, bl, false);
        let (r, g, b) = s.colour.zones[0].mean();
        let range = 1.0 - bl;
        assert!((r - range).abs() < 1e-12 && (g - range).abs() < 1e-12);
        assert!((b - range).abs() < 1e-12);
        let l = &s.luma.as_ref().unwrap().zones[0];
        assert_eq!(l.counted, 10);
        assert!((l.y / 10.0 - 0.5 * range).abs() < 1e-9);
        assert_eq!(s.histogram.total(), 20);
        // Bins 2 and 3 scaled by `range` stay in bins 2 and 3.
        assert_eq!(s.histogram.bins(), &[0, 0, 10, 10]);
        assert_eq!(rescale_histogram(&[0, 0, 10, 10], 0.5), vec![0, 20, 0, 0]);
        assert_eq!(rescale_histogram(&[5, 0], 4.0), vec![0, 5]);
    }
}
