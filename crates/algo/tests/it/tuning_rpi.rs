//! Raspberry Pi tuning conversion: a checked-in fixture, and every tuning file of a libcamera
//! and a HeliOS checkout next to this repository when present (skipped otherwise). Set
//! `STYX_LIBCAMERA_DIR` / `STYX_HELIOS_DIR` to point elsewhere.

use crate::common;

use std::path::{Path, PathBuf};

use styx_algo::sim::Scene;
use styx_algo::{Pipeline, Tuning};

const FIXTURE: &str = include_str!("../data/rpi-minimal.json");

#[test]
fn fixture_converts() {
    let import = Tuning::from_rpi_json_str(FIXTURE).unwrap();
    let t = &import.tuning;
    assert_eq!(import.target.as_deref(), Some("pisp"));
    assert_eq!(t.black_level.as_ref().unwrap().g, 4096.0 / 65536.0);
    assert_eq!(t.lux.unwrap().reference_y, 17000.0 / 65536.0);

    let agc = t.agc.as_ref().unwrap();
    assert_eq!(agc.default_metering_mode, "centre-weighted");
    assert_eq!(agc.metering_modes["spot"].weights.len(), 9);
    assert_eq!(agc.exposure_modes["short"].exposure_us[3], 20000.0);
    assert_eq!(agc.constraint_modes["highlight"].len(), 2);
    assert_eq!(agc.startup_frames, 5);

    let awb = t.awb.as_ref().unwrap();
    assert!(awb.uses_bayes());
    assert_eq!(awb.default_mode, "auto");
    assert_eq!(awb.ct_curve[1], [4600.0, 0.68, 0.64]);
    assert_eq!(awb.priors.len(), 2);

    let alsc = t.alsc.as_ref().unwrap();
    assert_eq!(alsc.grid, (16, 12));
    assert_eq!(alsc.calibrations_cr.len(), 2);
    assert_eq!(alsc.corner_strength, Some(2.0));

    let gamma = &t.contrast.as_ref().unwrap().gamma_curve;
    assert_eq!(gamma.domain(), (0.0, 1.0));
    assert!((gamma.eval(0.5) - 50000.0 / 65535.0).abs() < 1e-3);
    assert_eq!(t.ccm.as_ref().unwrap().ccms.len(), 2);

    let d = t.denoise.as_ref().unwrap();
    assert_eq!(d.noise.reference_slope, 3.0);
    assert_eq!(d.sharpen.unwrap().strength, 1.0);
    assert!(d.sdn.is_none() && d.tdn.is_none());

    let a = t.alsc.as_ref().unwrap();
    assert_eq!(a.omega, 1.3);
    assert_eq!(a.n_iter, Some(100));
    for key in ["rpi.awb.enabled", "rpi.agc.channels[1]"] {
        assert!(
            import.ignored.iter().any(|i| i == key),
            "{key}: {:?}",
            import.ignored
        );
    }
}

#[test]
fn fixture_round_trips_through_toml() {
    let t = Tuning::from_rpi_json_str(FIXTURE).unwrap().tuning;
    let text = t.to_toml_string().unwrap();
    assert_eq!(Tuning::from_toml_str(&text).unwrap(), t);
}

#[test]
fn rejects_other_versions_and_bad_values() {
    assert!(Tuning::from_rpi_json_str(r#"{"version": 1.0, "algorithms": []}"#).is_err());
    let bad = FIXTURE.replace("\"q_lo\": 0.98", "\"q_lo\": \"high\"");
    let e = Tuning::from_rpi_json_str(&bad).unwrap_err().to_string();
    assert!(e.contains("q_lo"), "{e}");
}

/// Convert, build the standard pipeline and run a few simulated frames.
fn exercise(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap();
    let import =
        Tuning::from_rpi_json_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let (mut p, mut sim) = common::start(
        &import.tuning,
        &common::config(),
        Scene::constant(300.0, 4500.0),
    );
    let frames = sim.run(&mut p, 12);
    let last = &frames.last().unwrap().params;
    assert!(last.sensor.is_some() && last.colour_gains.iter().all(|g| g.is_finite() && *g > 0.0));
    Pipeline::from_tuning(&import.tuning).unwrap();
    import.ignored
}

fn sibling(env: &str, name: &str) -> PathBuf {
    std::env::var_os(env).map(PathBuf::from).unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join(name)
    })
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    v.retain(|p| p.extension().is_some_and(|e| e == "json"));
    v.sort();
    v
}

#[test]
fn libcamera_tunings_convert() {
    let root = sibling("STYX_LIBCAMERA_DIR", "libcamera");
    let mut n = 0;
    for target in ["pisp", "vc4"] {
        for f in json_files(&root.join(format!("src/ipa/rpi/{target}/data"))) {
            exercise(&f);
            n += 1;
        }
    }
    if n == 0 {
        eprintln!("skipped: no libcamera checkout at {}", root.display());
    } else {
        println!("converted {n} Raspberry Pi tuning files");
    }
}

#[test]
fn helios_ov9782_tunings_convert() {
    let root = sibling("STYX_HELIOS_DIR", "HeliOS").join("gaia/assets/libcamera/ipa/rpi");
    let files: Vec<PathBuf> = ["pisp", "vc4"]
        .iter()
        .map(|t| root.join(t).join("ov9782.json"))
        .filter(|p| p.exists())
        .collect();
    if files.is_empty() {
        eprintln!("skipped: no HeliOS checkout at {}", root.display());
    }
    for f in files {
        let ignored = exercise(&f);
        println!("{}: ignored {ignored:?}", f.display());
    }
}

/// Import → export → import gives the same tuning (Styx-only settings stay at their defaults
/// through an import, so they survive too).
fn assert_round_trips(name: &str, text: &str) {
    let first = Tuning::from_rpi_json_str(text).unwrap().tuning;
    let json = first.to_rpi_json_string(None);
    let again = Tuning::from_rpi_json_str(&json)
        .unwrap_or_else(|e| panic!("{name}: re-import failed: {e}\n{json}"));
    assert!(again.ignored.is_empty(), "{name}: {:?}", again.ignored);
    assert_eq!(again.tuning, first, "{name}");
}

#[test]
fn export_round_trips() {
    assert_round_trips("fixture", FIXTURE);
    let ov9782 = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pipeline/tuning/ov9782.json");
    assert_round_trips("ov9782", &std::fs::read_to_string(ov9782).unwrap());
    let mut n = 0;
    let root = sibling("STYX_LIBCAMERA_DIR", "libcamera");
    for target in ["pisp", "vc4"] {
        for path in json_files(&root.join(format!("src/ipa/rpi/{target}/data"))) {
            let text = std::fs::read_to_string(&path).unwrap();
            assert_round_trips(&path.display().to_string(), &text);
            n += 1;
        }
    }
    println!("{n} libcamera tunings round-trip");
}

#[test]
fn export_keeps_what_the_import_reads() {
    let mut t = Tuning::from_rpi_json_str(FIXTURE).unwrap().tuning;
    // Values the Raspberry Pi IPA reads as integers are rounded on the way out.
    t.black_level = Some(styx_algo::tuning::BlackLevelTuning::uniform(64.2 / 1024.0));
    let json = t.to_rpi_json_string(Some("pisp"));
    assert!(json.contains("\"black_level\": 4109"), "{json}");
    assert!(json.contains("\"target\": \"pisp\""));
    let back = Tuning::from_rpi_json_str(&json).unwrap().tuning;
    assert_eq!(back.black_level.unwrap().g, 4109.0 / 65536.0);
}
