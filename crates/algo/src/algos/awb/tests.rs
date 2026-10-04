use super::*;
use crate::pwl::Pwl;
use crate::stats::StatsAccumulator;
use tuning::{AwbMode, AwbPrior};

/// A test tuning: a simple CT curve and a flat prior.
fn tuning() -> AwbTuning {
    AwbTuning {
        ct_curve: vec![[2500.0, 1.0, 0.4], [4500.0, 0.7, 0.6], [8000.0, 0.45, 0.85]],
        priors: vec![AwbPrior {
            lux: 0.0,
            prior: Pwl::constant(0.0),
        }],
        modes: [(
            "auto".to_string(),
            AwbMode {
                lo: 2500.0,
                hi: 8000.0,
            },
        )]
        .into(),
        transverse_pos: 0.02,
        transverse_neg: 0.02,
        min_regions: 4,
        ..Default::default()
    }
}

/// Grey surfaces under the curve's illuminant at `ct`, plus one red patch.
fn scene(awb: &AwbTuning, ct: f64) -> Statistics {
    let c = awb.curve().unwrap().unwrap();
    let (r, b) = (c.r.eval(ct), c.b.eval(ct));
    let mut a = StatsAccumulator::new(4, 4, 16, 1.0);
    for zy in 0..4 {
        for zx in 0..4 {
            let level = 0.1 + 0.05 * f64::from(zx + zy);
            let (pr, pg, pb) = if (zx, zy) == (1, 1) {
                (3.0, 0.3, 0.3)
            } else {
                (1.0, 1.0, 1.0)
            };
            for _ in 0..20 {
                a.add(zx, zy, level * r * pr, level * pg, level * b * pb);
            }
        }
    }
    a.finish()
}

fn meta() -> FrameMetadata {
    FrameMetadata::new(
        0,
        core::time::Duration::from_millis(10),
        1.0,
        core::time::Duration::from_millis(33),
    )
}

#[test]
fn bayes_finds_the_temperature() {
    for ct in [3000.0, 4500.0, 6500.0] {
        let t = tuning();
        let stats = scene(&t, ct);
        let mut awb = Awb::new(t).unwrap();
        awb.prepare(&CameraConfig::default()).unwrap();
        let mut p = Params::default();
        awb.process(&stats, &meta(), &mut p);
        let (est, r, _) = p.awb.estimate;
        assert!((est / ct - 1.0).abs() < 0.05, "{ct}: {est}");
        let c = awb.tuning.curve().unwrap().unwrap();
        assert!((r * c.r.eval(ct) - 1.0).abs() < 0.03);
        // Start-up: applied at once.
        assert_eq!(p.colour_gains[0], r);
    }
}

#[test]
fn too_few_zones_keep_the_previous_estimate() {
    let mut awb = Awb::new(tuning()).unwrap();
    awb.prepare(&CameraConfig::default()).unwrap();
    let mut p = Params::default();
    let before = awb.filtered;
    awb.process(&Statistics::default(), &meta(), &mut p);
    assert_eq!(awb.filtered, before);
}

#[test]
fn filtering_after_start_up_moves_by_speed() {
    let t = AwbTuning {
        startup_frames: 0,
        frame_period: 1,
        speed: 0.5,
        ..tuning()
    };
    let stats = scene(&t, 6500.0);
    let mut awb = Awb::new(t).unwrap();
    awb.prepare(&CameraConfig::default()).unwrap();
    let start = awb.filtered.1;
    let mut p = Params::default();
    awb.process(&stats, &meta(), &mut p);
    let target = p.awb.estimate.1;
    assert!((p.colour_gains[0] - (start + target) / 2.0).abs() < 1e-9);
    assert!(!p.awb.converged);
}
