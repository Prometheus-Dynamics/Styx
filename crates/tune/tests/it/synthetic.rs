//! End to end on a synthetic sensor: render dark frames, flat fields and ColorChecker shots
//! with known black level, lens shading, colour response and noise, calibrate, and require the
//! calibration to give back what the frames were made with.
//!
//! Run with `--nocapture` for the recovered-against-true tables.

use styx_tune::calib::{self, Calibration, Options, ccm};
use styx_tune::linalg::Mat3;
use styx_tune::session::{Kind, Shot};
use styx_tune::synth::{
    MatrixResponse, Response, SensorModel, Shot as Light, Target, chart_placement,
};
use styx_tune::{Tuning, chart};

const FRAMES: usize = 4;
/// Lux per unit of `Light::light` (e-/µs of a perfect white on axis).
const LUX_PER_LIGHT: f64 = 2000.0;

fn shot(
    name: &str,
    kind: Kind,
    ct: Option<f64>,
    lux: Option<f64>,
    frames: Vec<styx_tune::raw::RawFrame>,
) -> Shot {
    Shot {
        name: name.into(),
        kind,
        ct,
        lux,
        corners: None,
        frames,
    }
}

/// The exposure that puts a perfect white's brightest channel on axis at `level` of full
/// scale (the exposure rule of docs/tuning.md).
fn exposure_for(m: &SensorModel, ct: f64, light: f64, gain: f64, level: f64) -> f64 {
    let peak = m.white_rgb(ct).into_iter().fold(0.0, f64::max);
    level * f64::from(1u32 << m.bits) * m.electrons_per_code / (light * gain * peak)
}

fn placement() -> Mat3 {
    chart_placement(318.0, 204.0, 82.0, 4.0, 0.0002)
}

/// The usual session: dark at gains 1, 2, 4; flats at three temperatures; charts at four,
/// the 5000 K one at a known lux.
fn session(m: &SensorModel) -> (Vec<Shot>, f64) {
    let mut shots = Vec::new();
    for (i, gain) in [1.0, 2.0, 4.0].into_iter().enumerate() {
        let l = Light {
            ct: 5000.0,
            light: 0.0,
            exposure_us: 10_000.0,
            gain,
        };
        shots.push(shot(
            &format!("dark x{gain}"),
            Kind::Dark,
            None,
            None,
            m.burst(&Target::Dark, &l, FRAMES, 10 + i as u64),
        ));
    }
    for (i, ct) in [2800.0, 4000.0, 6500.0].into_iter().enumerate() {
        let light = 0.2;
        let l = Light {
            ct,
            light,
            exposure_us: exposure_for(m, ct, light, 1.0, 0.7),
            gain: 1.0,
        };
        shots.push(shot(
            &format!("flat {ct}"),
            Kind::Flat,
            Some(ct),
            None,
            m.burst(&Target::Flat(1.0), &l, FRAMES, 20 + i as u64),
        ));
    }
    let chart = Target::Chart {
        h: placement(),
        background: 0.25,
    };
    let lux_light = 0.15;
    for (i, ct) in [2800.0, 4000.0, 5000.0, 6500.0].into_iter().enumerate() {
        let light = if ct == 5000.0 { lux_light } else { 0.2 };
        let l = Light {
            ct,
            light,
            exposure_us: exposure_for(m, ct, light, 1.0, 0.75),
            gain: 1.0,
        };
        let lux = (ct == 5000.0).then_some(lux_light * LUX_PER_LIGHT);
        shots.push(shot(
            &format!("chart {ct}"),
            Kind::Macbeth,
            Some(ct),
            lux,
            m.burst(&chart, &l, FRAMES, 30 + i as u64),
        ));
    }
    (shots, lux_light)
}

fn run(shots: &[Shot]) -> Calibration {
    calib::calibrate(shots, &Tuning::default(), &Options::default()).unwrap()
}

/// Truth of the lens shading tables at cell centres, `(lum, cr, cb)` normalised as written.
fn shading_truth(m: &SensorModel, ct: f64, grid: (u32, u32)) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    let (w, h) = (m.width as f64, m.height as f64);
    let (mut lum, mut cr, mut cb) = (Vec::new(), Vec::new(), Vec::new());
    for j in 0..grid.1 {
        for i in 0..grid.0 {
            // Mean over the cell (the tool averages its pixels).
            let (mut g, mut r, mut b) = (0.0, 0.0, 0.0);
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = (f64::from(i) + (sx as f64 + 0.5) / 4.0) * w / f64::from(grid.0);
                    let y = (f64::from(j) + (sy as f64 + 0.5) / 4.0) * h / f64::from(grid.1);
                    g += m.shading.gain(1, x, y, w, h, ct);
                    r += m.shading.gain(0, x, y, w, h, ct);
                    b += m.shading.gain(2, x, y, w, h, ct);
                }
            }
            lum.push(1.0 / g);
            cr.push(g / r);
            cb.push(g / b);
        }
    }
    let norm = |v: Vec<f64>| {
        let min = v.iter().copied().fold(f64::INFINITY, f64::min);
        v.into_iter().map(|x| x / min).collect::<Vec<_>>()
    };
    (norm(lum), norm(cr), norm(cb))
}

/// What the lens shading's normalisation does to colour: the colour tables are normalised to a
/// smallest gain of 1, so (as at run time, where AWB sees the statistics through them) R/G and
/// B/G after correction are the light's divided by the smallest `G / R` (`G / B`) of the cells.
fn table_scale(m: &SensorModel, ct: f64) -> (f64, f64) {
    let (w, h) = (m.width as f64, m.height as f64);
    let (mut r, mut b) = (f64::INFINITY, f64::INFINITY);
    let grid = 32;
    for j in 0..grid {
        for i in 0..grid {
            let (mut sg, mut sr, mut sb) = (0.0, 0.0, 0.0);
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = (f64::from(i) + (sx as f64 + 0.5) / 4.0) * w / f64::from(grid);
                    let y = (f64::from(j) + (sy as f64 + 0.5) / 4.0) * h / f64::from(grid);
                    sg += m.shading.gain(1, x, y, w, h, ct);
                    sr += m.shading.gain(0, x, y, w, h, ct);
                    sb += m.shading.gain(2, x, y, w, h, ct);
                }
            }
            r = r.min(sg / sr);
            b = b.min(sg / sb);
        }
    }
    (r, b)
}

/// How much the calibrated colour tables at `ct` are scaled against the truth (their
/// normalisation by the smallest cell picks up that cell's noise): `(red, blue)`. The AWB curve
/// is measured through the tables, as AWB sees statistics at run time, so it carries the same
/// factor.
fn table_bias(cal: &Calibration, m: &SensorModel, ct: f64) -> (f64, f64) {
    let a = cal.alsc.as_ref().unwrap();
    let t = a.tables.iter().find(|t| (t.ct - ct).abs() < 1.0);
    let Some(t) = t else { return (1.0, 1.0) };
    let (_, cr, cb) = shading_truth(m, ct, a.grid);
    let k =
        |x: &[f64], y: &[f64]| x.iter().zip(y).map(|(p, q)| p / q).sum::<f64>() / x.len() as f64;
    (k(&t.cr, &cr), k(&t.cb, &cb))
}

/// The AWB point noise-free patches give through the calibration's own lens shading tables
/// (exactly what AWB sees at run time): `(R/G, B/G)`.
fn awb_oracle(cal: &Calibration, m: &SensorModel, ct: f64) -> (f64, f64) {
    let a = cal.alsc.as_ref().unwrap();
    let (pw, ph) = (m.width / 2, m.height / 2);
    let corr = a.correction(ct, pw, ph);
    let ideal = m.patch_rgb(ct);
    let (w, h) = (m.width as f64, m.height as f64);
    let rgb: [[f64; 3]; 24] = std::array::from_fn(|i| {
        let [x, y] = styx_tune::linalg::apply(&placement(), [(i % 6) as f64, (i / 6) as f64]);
        let g = corr.at(x / 2.0 - 0.25, y / 2.0 - 0.25);
        let v = |c| m.shading.gain(c, x, y, w, h, ct);
        [
            ideal[i][0] * v(0) * g[0],
            ideal[i][1] * v(1) * g[1],
            ideal[i][2] * v(2) * g[3],
        ]
    });
    let (wr, wb) = ccm::grey_gains(&rgb).unwrap();
    (1.0 / wr, 1.0 / wb)
}

fn mean_rel(a: &[f64], b: &[f64]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x / y - 1.0).abs())
        .sum::<f64>()
        / a.len() as f64
}

fn max_rel(a: &[f64], b: &[f64]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x / y - 1.0).abs())
        .fold(0.0, f64::max)
}

#[test]
fn matrix_sensor_parameters_come_back() {
    let m = SensorModel::default();
    let Response::Matrix(truth) = m.response.clone() else {
        unreachable!()
    };
    let (shots, lux_light) = session(&m);
    let cal = run(&shots);
    println!("{}", styx_tune::report::text(&cal));

    // Black level per gain, per channel: within 0.1 code.
    let b = cal.black.as_ref().unwrap();
    assert_eq!(b.by_gain.len(), 3);
    for g in &b.by_gain {
        let want = m.black_at(g.gain);
        for (c, w) in want.iter().enumerate() {
            let err = (g.levels[c] - w) * 1024.0;
            println!("black gain {} ch {c}: {:.3} codes off", g.gain, err);
            assert!(err.abs() < 0.1, "gain {} channel {c}: {err} codes", g.gain);
        }
    }
    let bt = cal.tuning.black_level.as_ref().unwrap();
    assert_eq!(bt.by_gain.len(), 3);
    assert!(
        m.hot_pixels.iter().all(|p| b.hot_pixels.contains(p)),
        "{:?}",
        b.hot_pixels
    );
    assert!(b.hot_pixels.len() <= m.hot_pixels.len() + 2);

    // Lens shading: the tables' shape within 0.3% of the truth on average and 1.5% in the
    // worst cell (a dim blue corner at 2800 K: ~60 codes, noise-limited); their scale within 1%.
    let a = cal.alsc.as_ref().unwrap();
    for t in &a.tables {
        let (lum, cr, cb) = shading_truth(&m, t.ct, a.grid);
        let (kr, kb) = table_bias(&cal, &m, t.ct);
        println!("alsc {} K: table scale {kr:.4} / {kb:.4}", t.ct);
        assert!((kr - 1.0).abs() < 0.01 && (kb - 1.0).abs() < 0.01);
        let (cr, cb): (Vec<f64>, Vec<f64>) = (
            cr.iter().map(|v| v * kr).collect(),
            cb.iter().map(|v| v * kb).collect(),
        );
        let (el, er, eb) = (
            max_rel(&a.luminance, &lum),
            max_rel(&t.cr, &cr),
            max_rel(&t.cb, &cb),
        );
        let (ml, mr, mb) = (
            mean_rel(&a.luminance, &lum),
            mean_rel(&t.cr, &cr),
            mean_rel(&t.cb, &cb),
        );
        println!(
            "alsc {} K: luminance {:.3}% mean / {:.3}% worst, Cr {:.3}% / {:.3}%, Cb {:.3}% / {:.3}%",
            t.ct,
            ml * 100.0,
            el * 100.0,
            mr * 100.0,
            er * 100.0,
            mb * 100.0,
            eb * 100.0
        );
        assert!(mr < 0.003 && mb < 0.003 && ml < 0.003, "{} K", t.ct);
        assert!(er < 0.015 && eb < 0.015 && el < 0.015, "{} K", t.ct);
    }

    // Chart found in every shot, corners within 1.5 pixels.
    for c in &cal.charts {
        assert!(!c.manual && c.usable >= 23, "{c:?}");
        for (k, i) in [0usize, 5, 23, 18].iter().enumerate() {
            let p = styx_tune::linalg::apply(&placement(), [(i % 6) as f64, (i / 6) as f64]);
            let e = ((c.corners[k][0] - p[0]).powi(2) + (c.corners[k][1] - p[1]).powi(2)).sqrt();
            assert!(e < 1.5, "{}: corner {k} {e} px off", c.shot);
        }
    }

    // AWB curve, seen through the lens shading tables as at run time: against the light's own
    // R/G and B/G (the chart's greys are not perfectly neutral, and 5000 K uses tables
    // interpolated between 4000 and 6500 K: within 1.5%, 2.5% at 5000 K) and against noise-free patches through
    // the calibrated tables (0.3%).
    // The curve itself is a quadratic fit through the points (in hat space, as ctt): it stays
    // within its transverse margins of them.
    let awb = cal.awb.as_ref().unwrap();
    assert_eq!(awb.curve.len(), 4);
    for (p, c) in awb.points.iter().zip(&awb.curve) {
        let (sr, sb) = table_scale(&m, p.ct);
        let (kr, kb) = table_bias(&cal, &m, p.ct);
        let (r, b) = truth.curve(p.ct);
        let (r, b) = (r / sr * kr, b / sb * kb);
        let (ir, ib) = awb_oracle(&cal, &m, p.ct);
        println!(
            "awb {} K: r {:.4} (light {r:.4}, noise-free {ir:.4}, curve {:.4}), b {:.4} (light {b:.4}, noise-free {ib:.4}, curve {:.4})",
            p.ct, p.r, c[1], p.b, c[2]
        );
        let tol = if p.ct == 5000.0 { 0.025 } else { 0.015 };
        assert!((p.r / r - 1.0).abs() < tol && (p.b / b - 1.0).abs() < tol);
        assert!((p.r / ir - 1.0).abs() < 0.003 && (p.b / ib - 1.0).abs() < 0.003);
        let off = ((c[1] - p.r).powi(2) + (c[2] - p.b).powi(2)).sqrt();
        assert!(
            off <= awb.transverse_pos.max(awb.transverse_neg) + 1e-4,
            "{off}"
        );
    }

    // Colour matrices: within 0.03 of the true matrix (white balance on the chart's greys,
    // as ctt does, moves them a little) and 0.015 of the fit on noise-free patches.
    assert_eq!(cal.ccms.len(), 4);
    for f in &cal.ccms {
        let t = truth.ccm(f.ct);
        let rgb: [[f64; 3]; 24] = m.patch_rgb(f.ct).try_into().unwrap();
        let mut usable = [true; 24];
        usable[17] = f.patch_de[17] > 0.0 || usable[17];
        let oracle = ccm::fit(f.ct, &rgb, &usable, 4.0).unwrap();
        let et = f
            .ccm
            .iter()
            .zip(&t)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        let eo = f
            .ccm
            .iter()
            .zip(&oracle.ccm)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        println!(
            "ccm {} K: max |Δ| {et:.4} to truth, {eo:.4} to noise-free fit; ΔE76 {:.2} (noise-free {:.2})",
            f.ct, f.mean_de, oracle.mean_de
        );
        assert!(et < 0.03 && eo < 0.015);
        assert!(f.mean_de < 1.0);
    }

    // Noise: shot noise of 2.5 e-/code, read noise 0.7 codes, quantisation; the model
    // `c + s √L` is close to that `√(a + b L)` over the range.
    let (n, source) = cal.noise.as_ref().unwrap();
    assert_eq!(*source, "temporal");
    for level in [1000.0, 5000.0, 20000.0, 45000.0] {
        let fit = n.constant + n.slope * f64::sqrt(level);
        let want = m.noise_16bit(level);
        println!("noise at {level}: fit {fit:.1}, true {want:.1}");
        assert!((fit / want - 1.0).abs() < 0.04, "{level}: {fit} vs {want}");
    }

    // Lux: the reference describes its own shot.
    let lux = cal.lux.unwrap();
    assert_eq!(lux.reference_lux, lux_light * LUX_PER_LIGHT);
    assert!((lux.reference_gain - 1.0).abs() < 1e-9);

    // The tuning loads, converts and runs.
    let t = &cal.tuning;
    t.validate().unwrap();
    let again = Tuning::from_toml_str(&t.to_toml_string().unwrap()).unwrap();
    assert_eq!(&again, t);
    let rpi = Tuning::from_rpi_json_str(&t.to_rpi_json_string(None)).unwrap();
    assert_eq!(rpi.tuning.ccm, t.ccm);
    assert_eq!(
        rpi.tuning.awb.as_ref().unwrap().ct_curve,
        t.awb.as_ref().unwrap().ct_curve
    );
    styx_algo::Pipeline::from_tuning(t).unwrap();
}

/// The lux reference predicts the illuminance of a shot at another light and exposure.
#[test]
fn lux_reference_predicts_other_shots() {
    let m = SensorModel {
        hot_pixels: Vec::new(),
        ..SensorModel::default()
    };
    let chart = Target::Chart {
        h: placement(),
        background: 0.25,
    };
    let reference = |light: f64, gain: f64, seed| {
        let l = Light {
            ct: 5000.0,
            light,
            exposure_us: exposure_for(&m, 5000.0, light, gain, 0.6),
            gain,
        };
        let s = shot(
            "chart",
            Kind::Macbeth,
            Some(5000.0),
            Some(light * LUX_PER_LIGHT),
            m.burst(&chart, &l, 2, seed),
        );
        run(&[s]).lux.unwrap()
    };
    let a = reference(0.2, 1.0, 1);
    let b = reference(0.05, 2.0, 2);
    // Estimate b's lux from a's reference, as styx-algo's Lux does.
    let est = a.reference_lux
        * (a.reference_exposure_us / b.reference_exposure_us)
        * (a.reference_gain / b.reference_gain)
        * (b.reference_y / a.reference_y);
    println!("lux: estimated {est:.1}, true {:.1}", b.reference_lux);
    assert!((est / b.reference_lux - 1.0).abs() < 0.02);
}

/// A spectral sensor (Gaussian sensitivities, black-body light): no exact matrix exists, so the
/// result must match what noise-free, unshaded patches give.
#[test]
fn spectral_sensor_matches_the_noise_free_fit() {
    let m = SensorModel {
        response: Response::Spectral(Default::default()),
        hot_pixels: Vec::new(),
        ..SensorModel::default()
    };
    let (shots, _) = session(&m);
    let cal = run(&shots);
    let awb = cal.awb.as_ref().unwrap();
    for p in &awb.points {
        let (ir, ib) = awb_oracle(&cal, &m, p.ct);
        let white = m.white_rgb(p.ct);
        println!(
            "spectral awb {} K: r {:.4} (noise-free {ir:.4}; perfect white, unshaded {:.4}) b {:.4} (noise-free {ib:.4}; white {:.4})",
            p.ct, p.r, white[0], p.b, white[2]
        );
        assert!((p.r / ir - 1.0).abs() < 0.003 && (p.b / ib - 1.0).abs() < 0.003);
    }
    for f in &cal.ccms {
        let rgb: [[f64; 3]; 24] = m.patch_rgb(f.ct).try_into().unwrap();
        let oracle = ccm::fit(f.ct, &rgb, &[true; 24], 4.0).unwrap();
        let eo = f
            .ccm
            .iter()
            .zip(&oracle.ccm)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        println!(
            "spectral ccm {} K: {eo:.4} from the noise-free fit; ΔE76 {:.2} (noise-free {:.2}, no matrix {:.2})",
            f.ct, f.mean_de, oracle.mean_de, f.identity_de
        );
        assert!(eo < 0.015, "{:?} vs {:?}", f.ccm, oracle.ccm);
        assert!(f.mean_de < oracle.mean_de + 0.3 && f.mean_de < f.identity_de);
    }
}

/// Corners given by hand place the chart when detection cannot; a shot with no chart says so.
#[test]
fn manual_corners_and_missing_charts() {
    let m = SensorModel {
        hot_pixels: Vec::new(),
        ..SensorModel::default()
    };
    let h = placement();
    let l = Light {
        ct: 5000.0,
        light: 0.2,
        exposure_us: exposure_for(&m, 5000.0, 0.2, 1.0, 0.75),
        gain: 1.0,
    };
    let frames = m.burst(
        &Target::Chart {
            h,
            background: 0.25,
        },
        &l,
        2,
        5,
    );
    let mut s = shot("chart", Kind::Macbeth, Some(5000.0), None, frames);
    let auto = run(std::slice::from_ref(&s));
    // Corners within a few pixels of the truth give the same matrix within 0.005.
    let truth =
        [0usize, 5, 23, 18].map(|i| styx_tune::linalg::apply(&h, [(i % 6) as f64, (i / 6) as f64]));
    s.corners = Some(truth.map(|p| [p[0] + 2.0, p[1] - 1.5]));
    let manual = run(std::slice::from_ref(&s));
    assert!(manual.charts[0].manual);
    let d = auto.ccms[0]
        .ccm
        .iter()
        .zip(&manual.ccms[0].ccm)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f64::max);
    assert!(d < 0.005, "{d}");
    // A flat field has no chart.
    let flat = shot(
        "flat",
        Kind::Macbeth,
        Some(5000.0),
        None,
        m.burst(&Target::Flat(1.0), &l, 1, 6),
    );
    let none = run(&[flat]);
    assert!(
        none.ccms.is_empty() && none.notes.iter().any(|n| n.contains("no chart found")),
        "{:?}",
        none.notes
    );
    assert!(
        chart::detect(&styx_tune::raw::Planes::from_frame(&m.render(
            &Target::Dark,
            &l,
            &mut styx_tune::synth::Rng::new(1)
        )))
        .is_none()
    );
}

#[test]
fn matrix_response_truth_is_consistent() {
    let t = MatrixResponse::default();
    assert_eq!(t.curve(2800.0), (0.95, 0.42));
    for (_, m) in &t.ccms {
        for r in 0..3 {
            assert!((m[r * 3] + m[r * 3 + 1] + m[r * 3 + 2] - 1.0).abs() < 1e-12);
        }
    }
}
