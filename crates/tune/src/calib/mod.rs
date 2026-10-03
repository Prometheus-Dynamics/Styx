//! The calibration: every step run over a session's shots, assembled into a [`Tuning`].
//!
//! Order: black level (dark shots; else the files' own level, else `Options::black_level`)
//! → per shot, the mean of its frames with the black level removed → lens shading (flats) →
//! charts located and sampled, lens shading corrected → AWB curve (chart greys, grey cards) and
//! colour matrices (charts) → noise profile (bursts, else chart patches) → lux (the chart with
//! a lux value) → green equalisation (flats). Sections without data are kept from the base
//! tuning, and [`Calibration::notes`] says why.

pub mod alsc;
pub mod awb;
pub mod black;
pub mod ccm;
pub mod noise;

use styx_algo::Pwl;
use styx_algo::tuning::{
    AlscCalibration, AwbMode, AwbPrior, BlackLevelTuning, CtCcm, GainBlackLevel, GeqTuning,
    LuxTuning, NoiseTuning,
};

use crate::Tuning;
use crate::chart::{Chart, Patch, detect};
use crate::colour::WB_GREYS;
use crate::error::Result;
use crate::raw::{B, Burst, GB, GR, R};
use crate::session::{Kind, Shot};

/// Settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    /// Lens shading table grid (32×32 for the PiSP, 16×12 for VC4 and the software ISP's
    /// statistics).
    pub grid: (u32, u32),
    /// ALSC `luminance_strength` written (how much of the vignetting the ISP corrects).
    pub luminance_strength: f64,
    /// Largest magnitude of a colour matrix coefficient.
    pub max_ccm_coefficient: f64,
    /// Black level when there are no dark shots and the files state none, normalised.
    pub black_level: f64,
    /// Description written into the tuning.
    pub description: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            grid: (32, 32),
            luminance_strength: 0.8,
            max_ccm_coefficient: 4.0,
            black_level: 4096.0 / 65536.0,
            description: None,
        }
    }
}

/// Where a chart was found in a shot.
#[derive(Clone, Debug, PartialEq)]
pub struct ChartReport {
    /// The shot.
    pub shot: String,
    /// Corner patch centres (dark skin, bluish green, black, white), full-resolution pixels.
    pub corners: [[f64; 2]; 4],
    /// Placed from given corners.
    pub manual: bool,
    /// Patches found as regions (automatic detection).
    pub found: usize,
    /// Patches usable (no clipped or near-black channel).
    pub usable: usize,
}

/// Everything measured, and the tuning made from it.
#[derive(Clone, Debug)]
pub struct Calibration {
    /// The tuning (the base tuning with the measured sections replaced).
    pub tuning: Tuning,
    /// Black level.
    pub black: Option<black::BlackResult>,
    /// Lens shading.
    pub alsc: Option<alsc::AlscResult>,
    /// AWB curve.
    pub awb: Option<awb::AwbResult>,
    /// One matrix fit per chart.
    pub ccms: Vec<ccm::CcmFit>,
    /// Noise profile and its source (`temporal` or `chart patches`).
    pub noise: Option<(noise::NoiseFit, &'static str)>,
    /// Noise samples offered to the fit.
    pub noise_samples: Vec<noise::NoiseSample>,
    /// Lux reference.
    pub lux: Option<LuxTuning>,
    /// Green equalisation.
    pub geq: Option<GeqTuning>,
    /// Charts.
    pub charts: Vec<ChartReport>,
    /// What could not be calibrated, and warnings.
    pub notes: Vec<String>,
}

struct Prepared<'a> {
    shot: &'a Shot,
    burst: Burst,
    /// Mean planes, black removed.
    planes: crate::raw::Planes,
    black: [f64; 4],
}

/// Run the calibration over `shots`; sections not measured come from `base`.
pub fn calibrate(shots: &[Shot], base: &Tuning, opts: &Options) -> Result<Calibration> {
    let mut notes = Vec::new();
    // Black level.
    let dark: Vec<&crate::raw::RawFrame> = shots
        .iter()
        .filter(|s| s.kind == Kind::Dark)
        .flat_map(|s| &s.frames)
        .collect();
    let black = (!dark.is_empty()).then(|| black::calibrate(&dark));
    if black.is_none() {
        notes.push("black level: no dark shots; using the files' level or the default".into());
    }
    let black_for = |s: &Shot| -> [f64; 4] {
        let f = &s.frames[0];
        if let Some(l) = black.as_ref().and_then(|b| b.at(f.analogue_gain)) {
            return l;
        }
        [f.black_level.unwrap_or(opts.black_level); 4]
    };
    // Every other shot as the mean of its frames.
    let mut prepared = Vec::new();
    for s in shots.iter().filter(|s| s.kind != Kind::Dark) {
        let bl = black_for(s);
        let Some(burst) = Burst::new(&s.frames, bl.iter().sum::<f64>() / 4.0) else {
            notes.push(format!("{}: frames of different sizes; skipped", s.name));
            continue;
        };
        let mut planes = burst.mean.clone();
        planes.subtract(bl);
        prepared.push(Prepared {
            shot: s,
            burst,
            planes,
            black: bl,
        });
    }
    // Lens shading.
    let flats: Vec<(f64, &crate::raw::Planes)> = prepared
        .iter()
        .filter(|p| p.shot.kind == Kind::Flat)
        .filter_map(|p| Some((p.shot.ct?, &p.planes)))
        .collect();
    let alsc = alsc::calibrate(&flats, opts.grid);
    match &alsc {
        Some(a) => notes.extend(a.warnings.iter().cloned()),
        None => notes
            .push("lens shading: no flat shots; charts measured without shading correction".into()),
    }
    // Charts, grey cards, noise.
    let mut awb_points = Vec::new();
    let mut ccms = Vec::new();
    let mut charts = Vec::new();
    let mut temporal = Vec::new();
    let mut spatial = Vec::new();
    let mut lux = None;
    let mut geq_points: Vec<(f64, f64)> = Vec::new();
    for p in &prepared {
        let s = p.shot;
        let white = 1.0 - p.black.iter().copied().fold(0.0, f64::max);
        let known = p.burst.exposure_us > 0.0;
        if p.burst.variance.is_some() && known {
            temporal.extend(noise::temporal_samples(&p.burst, &p.planes));
        } else if p.burst.variance.is_some() {
            notes.push(format!(
                "{}: exposure and gain not recorded (give them in the session); not used for noise",
                s.name
            ));
        }
        let correction = match (&alsc, s.ct) {
            (Some(a), Some(ct)) => Some(a.correction(ct, p.planes.width, p.planes.height)),
            _ => None,
        };
        match s.kind {
            Kind::Flat => {
                for c in alsc::cell_means(&p.planes, (16, 12)) {
                    let g = (c[GR] + c[GB]) / 2.0;
                    if g > 0.01 && g < 0.9 * white {
                        geq_points.push((g * 65536.0, (c[GR] - c[GB]).abs() * 65536.0));
                    }
                }
            }
            Kind::Grey => {
                let (w, h) = (p.planes.width, p.planes.height);
                let Some(m) = p.planes.region(w * 2 / 5, h * 2 / 5, w * 3 / 5, h * 3 / 5) else {
                    continue;
                };
                let m = match &correction {
                    Some(c) => c.apply([w as f64 / 2.0, h as f64 / 2.0], m),
                    None => m,
                };
                let g = (m[GR] + m[GB]) / 2.0;
                awb_points.push(awb::AwbPoint {
                    ct: s.ct.unwrap_or_default(),
                    r: m[R] / g,
                    b: m[B] / g,
                    source: s.name.clone(),
                });
            }
            Kind::Macbeth => {
                let (chart, found) = match s.corners {
                    Some(c) => (Chart::from_corners(c), 0),
                    None => match detect(&p.planes) {
                        Some(d) => (Some(d.chart), d.found),
                        None => (None, 0),
                    },
                };
                let Some(chart) = chart else {
                    notes.push(format!(
                        "{}: no chart found; give its corner patches (`corners`) in the session",
                        s.name
                    ));
                    continue;
                };
                let patches = chart.sample(&p.planes, p.burst.variance.as_ref());
                let usable: [bool; 24] = std::array::from_fn(|i| {
                    let m = patches[i].mean;
                    m.iter().all(|&v| v > 0.002)
                        && m.iter().zip(&p.black).all(|(v, b)| v + b < 0.95)
                });
                let n_usable = usable.iter().filter(|u| **u).count();
                charts.push(ChartReport {
                    shot: s.name.clone(),
                    corners: [0, 5, 23, 18].map(|i| chart.centre_full(i)),
                    manual: chart.manual,
                    found,
                    usable: n_usable,
                });
                if WB_GREYS.clone().any(|i| !usable[i]) {
                    notes.push(format!(
                        "{}: a middle grey patch is clipped or black; skipped",
                        s.name
                    ));
                    continue;
                }
                let rgb: [[f64; 3]; 24] = std::array::from_fn(|i| {
                    let m = match &correction {
                        Some(c) => c.apply(patches[i].centre, patches[i].mean),
                        None => patches[i].mean,
                    };
                    Patch {
                        mean: m,
                        ..patches[i]
                    }
                    .rgb()
                });
                let ct = s.ct.unwrap_or_default();
                if let Some((wr, wb)) = ccm::grey_gains(&rgb) {
                    awb_points.push(awb::AwbPoint {
                        ct,
                        r: 1.0 / wr,
                        b: 1.0 / wb,
                        source: s.name.clone(),
                    });
                    if let Some(l) = s.lux.filter(|_| known) {
                        lux = Some(lux_reference(p, (wr, wb), l));
                    }
                }
                match ccm::fit(ct, &rgb, &usable, opts.max_ccm_coefficient) {
                    Some(f) => ccms.push(f),
                    None => notes.push(format!(
                        "{}: too few usable patches for a colour matrix",
                        s.name
                    )),
                }
                for (q, u) in patches.iter().zip(&usable) {
                    if !u || !known {
                        continue;
                    }
                    for c in 0..4 {
                        spatial.push(noise::NoiseSample {
                            level: q.mean[c] * 65536.0,
                            sigma: q.variance[c].sqrt() * 65536.0,
                            gain: p.burst.gain(),
                            count: q.count,
                        });
                    }
                    let g = (q.mean[GR] + q.mean[GB]) / 2.0;
                    geq_points.push((g * 65536.0, (q.mean[GR] - q.mean[GB]).abs() * 65536.0));
                }
            }
            Kind::Noise | Kind::Dark => {}
        }
    }
    let awb = awb::calibrate(&awb_points);
    if awb.is_none() {
        notes.push(format!(
            "AWB curve: needs greys at two colour temperatures or more (have {})",
            awb_points.len()
        ));
    }
    if ccms.is_empty() {
        notes.push("colour matrices: no usable chart".into());
    }
    let (noise_fit, noise_samples) = match noise::fit(&temporal) {
        Some(f) => (Some((f, "temporal")), temporal),
        None => (noise::fit(&spatial).map(|f| (f, "chart patches")), spatial),
    };
    if noise_fit.is_none() {
        notes.push("noise profile: needs a burst of frames of a static scene, or a chart".into());
    }
    if lux.is_none() {
        notes.push("lux: no chart shot with a lux value".into());
    }
    let geq = geq_fit(&geq_points);
    let mut cal = Calibration {
        tuning: base.clone(),
        black,
        alsc,
        awb,
        ccms,
        noise: noise_fit,
        noise_samples,
        lux,
        geq,
        charts,
        notes,
    };
    cal.tuning = assemble(&cal, base, opts, shots);
    cal.tuning.validate()?;
    Ok(cal)
}

/// The lux reference: the shot's mean luma as the AGC statistics see it (Rec. 601 luma of the
/// white balanced channels, green gain 1, lens shading not corrected), with its exposure.
fn lux_reference(p: &Prepared, (wr, wb): (f64, f64), lux: f64) -> LuxTuning {
    let m = p.planes.mean();
    let y = 0.299 * m[R] * wr + 0.587 * (m[GR] + m[GB]) / 2.0 + 0.114 * m[B] * wb;
    LuxTuning {
        reference_exposure_us: p.burst.exposure_us,
        reference_gain: p.burst.gain(),
        reference_aperture: 1.0,
        reference_lux: lux,
        reference_y: y,
    }
}

/// Green equalisation: the Gr/Gb difference against level, as a line every point stays under
/// (least-squares slope, offset raised to the 99th percentile), 16-bit scale.
fn geq_fit(points: &[(f64, f64)]) -> Option<GeqTuning> {
    if points.len() < 10 {
        return None;
    }
    let xs: Vec<f64> = points.iter().map(|p| p.0).collect();
    let ys: Vec<f64> = points.iter().map(|p| p.1).collect();
    let k = crate::linalg::polyfit(&xs, &ys, 1)?;
    let slope = k[1].max(0.0);
    let mut above: Vec<f32> = points.iter().map(|p| (p.1 - slope * p.0) as f32).collect();
    above.sort_by(f32::total_cmp);
    let offset = f64::from(above[((above.len() - 1) as f64 * 0.99) as usize]).max(0.0);
    Some(GeqTuning {
        offset: offset.round(),
        slope: (slope * 1e5).round() / 1e5,
        strength: None,
    })
}

/// The base tuning with every measured section replaced.
fn assemble(c: &Calibration, base: &Tuning, opts: &Options, shots: &[Shot]) -> Tuning {
    let mut t = base.clone();
    let kinds = |k: Kind| shots.iter().filter(|s| s.kind == k).count();
    t.description = Some(opts.description.clone().unwrap_or_else(|| {
        format!(
            "styx-tune: {} dark, {} flat, {} chart, {} grey, {} noise shot(s)",
            kinds(Kind::Dark),
            kinds(Kind::Flat),
            kinds(Kind::Macbeth),
            kinds(Kind::Grey),
            kinds(Kind::Noise)
        )
    }));
    if let Some(b) = &c.black {
        let lv = |l: &[f64; 4]| (l[R], (l[GR] + l[GB]) / 2.0, l[B]);
        let base_levels = b.at(1.0).unwrap_or([opts.black_level; 4]);
        let (r, g, bl) = lv(&base_levels);
        t.black_level = Some(BlackLevelTuning {
            r,
            g,
            b: bl,
            by_gain: if b.by_gain.len() > 1 {
                b.by_gain
                    .iter()
                    .map(|e| {
                        let (r, g, b) = lv(&e.levels);
                        GainBlackLevel {
                            gain: e.gain,
                            r,
                            g,
                            b,
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            },
        });
    }
    if let Some(a) = &c.alsc {
        let mut s = t.alsc.clone().unwrap_or_default();
        let round = |v: &Vec<f64>| v.iter().map(|x| (x * 1000.0).round() / 1000.0).collect();
        s.grid = a.grid;
        s.calibrations_cr = a
            .tables
            .iter()
            .map(|e| AlscCalibration {
                ct: e.ct,
                table: round(&e.cr),
            })
            .collect();
        s.calibrations_cb = a
            .tables
            .iter()
            .map(|e| AlscCalibration {
                ct: e.ct,
                table: round(&e.cb),
            })
            .collect();
        s.luminance_lut = round(&a.luminance);
        s.corner_strength = None;
        s.luminance_strength = opts.luminance_strength;
        s.sigma_cr = a.sigma_cr;
        s.sigma_cb = a.sigma_cb;
        s.default_ct = a.tables[a.tables.len() / 2].ct;
        t.alsc = Some(s);
    }
    if let Some(a) = &c.awb {
        let mut s = t.awb.clone().unwrap_or_default();
        s.bayes = true;
        s.ct_curve = a.curve.clone();
        s.transverse_pos = a.transverse_pos;
        s.transverse_neg = a.transverse_neg;
        if s.priors.is_empty() {
            s.priors = awb::default_priors()
                .into_iter()
                .filter_map(|(lux, p)| {
                    Some(AwbPrior {
                        lux,
                        prior: Pwl::from_flat(&p).ok()?,
                    })
                })
                .collect();
        }
        if s.modes.is_empty() {
            let (lo, hi) = (a.curve[0][0], a.curve[a.curve.len() - 1][0]);
            let modes = awb::default_modes(lo, hi);
            s.default_mode = modes[0].0.into();
            s.modes = modes
                .into_iter()
                .map(|(n, lo, hi)| (n.into(), AwbMode { lo, hi }))
                .collect();
        }
        t.awb = Some(s);
    }
    if !c.ccms.is_empty() {
        let mut s = t.ccm.clone().unwrap_or_default();
        s.ccms = ccm::combine(&c.ccms)
            .into_iter()
            .map(|(ct, ccm)| CtCcm { ct, ccm })
            .collect();
        t.ccm = Some(s);
    }
    if c.noise.is_some()
        || c.geq.is_some()
        || c.black.as_ref().is_some_and(|b| !b.hot_pixels.is_empty())
    {
        let mut d = t.denoise.clone().unwrap_or_default();
        if let Some((n, _)) = &c.noise {
            d.noise = NoiseTuning {
                reference_constant: n.constant,
                reference_slope: n.slope,
            };
        }
        if let Some(g) = &c.geq {
            d.geq = Some(g.clone());
        }
        if c.black.as_ref().is_some_and(|b| !b.hot_pixels.is_empty()) && d.dpc == 0 {
            d.dpc = 1;
        }
        t.denoise = Some(d);
    }
    if let Some(l) = c.lux {
        t.lux = Some(l);
    }
    t
}
