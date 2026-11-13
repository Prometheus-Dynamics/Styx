//! The tuning this crate writes is the tuning Styx's search reads: the written file loads through
//! `styx-tuning`'s search and gives back the calibrated values; a session on disk calibrates
//! like the frames it was written from; the bundled directory and bad names are refused.

use std::path::{Path, PathBuf};
use std::process::Command;

use styx_tune::calib::{self, Options};
use styx_tune::session::SessionFile;
use styx_tune::synth::{SensorModel, session};
use styx_tune::{Tuning, output};
use styx_tuning::{BUNDLED_DIR, Origin, Source, TuningSearch};

/// A fresh directory under the target directory (not the system temp directory).
fn scratch(name: &str) -> PathBuf {
    let p = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn written_tuning_is_found_by_the_search_with_the_calibrated_values() {
    let m = SensorModel::default();
    let (shots, _) = session::standard_session(&m);
    let cal = calib::calibrate(&shots, &Tuning::default(), &Options::default()).unwrap();
    let added = scratch("search");
    let path = output::write(&added, "fixture", &cal.tuning).unwrap();
    assert_eq!(path, added.join("fixture.toml"));

    let found = TuningSearch::new([added.clone()], None).find(Some("fixture.toml"));
    assert!(
        matches!(
            found.source,
            Source::File {
                origin: Origin::Added,
                ..
            }
        ),
        "{}",
        found.describe()
    );
    assert!(found.skipped.is_empty(), "{}", found.describe());
    assert_eq!(found.tuning, cal.tuning);

    // The black levels it holds are the sensor's, within a tenth of a code.
    let bl = found.tuning.black_level.as_ref().unwrap();
    assert_eq!(bl.by_gain.len(), 3);
    for g in &bl.by_gain {
        let want = m.black_at(g.gain);
        for (got, w) in [(g.r, want[0]), (g.b, want[3])] {
            let err = (got - w) * 1024.0;
            assert!(err.abs() < 0.1, "gain {}: {err} codes", g.gain);
        }
    }
    // The lens shading tables made it in: one per flat-field temperature (three in the session).
    let alsc = found.tuning.alsc.as_ref().unwrap();
    assert_eq!(alsc.calibrations_cr.len(), 3);
    assert_eq!(alsc.calibrations_cb.len(), 3);
}

#[test]
fn a_session_on_disk_calibrates_like_the_frames_it_was_written_from() {
    let m = SensorModel::default();
    let (shots, _) = session::standard_session(&m);
    let dir = scratch("session");
    let path = session::write_session(&dir, "fixture", &shots).unwrap();
    let file = SessionFile::read(&path).unwrap();
    assert_eq!(file.sensor.as_deref(), Some("fixture"));
    let loaded = file.load(&dir).unwrap();
    assert_eq!(loaded.len(), shots.len());

    let a = calib::calibrate(&shots, &Tuning::default(), &Options::default()).unwrap();
    let b = calib::calibrate(&loaded, &Tuning::default(), &Options::default()).unwrap();
    assert_eq!(a.tuning, b.tuning);
}

#[test]
fn refuses_the_bundled_directory_and_bad_names() {
    let t = Tuning::default();
    assert!(output::write(Path::new(BUNDLED_DIR), "x", &t).is_err());
    assert!(!Path::new(BUNDLED_DIR).join("x.toml").exists());
    let dir = scratch("names");
    for bad in ["", "../x", "a/b", "x.toml", "."] {
        assert!(output::write(&dir, bad, &t).is_err(), "{bad:?} accepted");
    }
    assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
}

#[test]
fn the_command_line_writes_a_tuning_the_search_finds() {
    let work = scratch("cli");
    let bin = env!("CARGO_BIN_EXE_styx-tune");
    let synth = Command::new(bin).arg("synth").arg(&work).output().unwrap();
    assert!(
        synth.status.success(),
        "{}",
        String::from_utf8_lossy(&synth.stderr)
    );

    let added = work.join("added");
    let cal = Command::new(bin)
        .arg("calibrate")
        .arg(work.join("session.toml"))
        .arg("--out")
        .arg(&added)
        .output()
        .unwrap();
    assert!(
        cal.status.success(),
        "{}",
        String::from_utf8_lossy(&cal.stderr)
    );
    let stdout = String::from_utf8_lossy(&cal.stdout);
    assert!(stdout.contains("wrote"), "{stdout}");

    // The sensor is named `synthetic` in its session.
    let found = TuningSearch::new([added], None).find(Some("synthetic.toml"));
    assert!(!found.is_fallback(), "{}", found.describe());
    assert!(found.tuning.black_level.is_some() && found.tuning.ccm.is_some());
}
