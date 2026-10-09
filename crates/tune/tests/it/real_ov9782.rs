//! The real OV9782 on the CM5, where its captures are present (skipped otherwise): the
//! `styx-tune capture` session in `target/device-tuning` (black series at gains 1-15.5, static
//! scene bursts at each gain; `STYX_TUNE_DEVICE_DIR` to point elsewhere) and a raw recording of
//! an earlier session in a sibling worktree. No chart or flat field was in view, so only the
//! black level and the noise profile can be checked; docs/tuning.md has the comparison with the
//! existing `ov9782.json`.

use std::path::{Path, PathBuf};

use styx_tune::Tuning;
use styx_tune::calib::{self, Options};
use styx_tune::input::Loader;
use styx_tune::session::{Kind, SessionFile, Shot};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
#[cfg(feature = "mcap")]
fn device_session_black_level_and_noise() {
    let dir = std::env::var_os("STYX_TUNE_DEVICE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("target/device-tuning"));
    let Ok(text) = std::fs::read_to_string(dir.join("session.toml")) else {
        eprintln!("skipped: no device session at {}", dir.display());
        return;
    };
    let shots = SessionFile::parse(&text)
        .unwrap()
        .load(&dir, &Loader::new())
        .unwrap();
    let cal = calib::calibrate(&shots, &Tuning::default(), &Options::default()).unwrap();
    println!("{}", styx_tune::report::text(&cal));
    // Raw level at zero exposure, 2026-10-02 (pipeline.md): 64.74-65.34 codes at 1x.
    let b = cal.black.as_ref().unwrap();
    let one = b.by_gain.iter().find(|g| g.gain == 1.0).unwrap();
    for l in one.levels {
        assert!(
            (l * 1024.0 - 65.0).abs() < 1.0,
            "{:?}",
            one.levels.map(|v| v * 1024.0)
        );
    }
    let (n, source) = cal.noise.as_ref().unwrap();
    assert_eq!(*source, "temporal");
    assert!((2.0..4.0).contains(&n.slope) && n.rms_error < 0.08, "{n:?}");
}

#[test]
fn earlier_recording_noise_at_gain_2() {
    let base = root().join("../Styx-native-quality2/target/q2/nraw.jsonl");
    if !base.exists() {
        eprintln!("skipped: no recording at {}", base.display());
        return;
    }
    let frames = Loader::new().load(&base).unwrap();
    let shot = Shot {
        name: "nraw".into(),
        kind: Kind::Noise,
        ct: None,
        lux: None,
        corners: None,
        // The first frames ran at other settings.
        frames: frames
            .into_iter()
            .filter(|f| f.analogue_gain == 2.0)
            .collect(),
    };
    let cal = calib::calibrate(&[shot], &Tuning::default(), &Options::default()).unwrap();
    let (n, _) = cal.noise.as_ref().unwrap();
    println!(
        "nraw (20 ms x 2, 17 frames): constant {} slope {}",
        n.constant, n.slope
    );
    assert!((2.0..4.0).contains(&n.slope), "{n:?}");
}
