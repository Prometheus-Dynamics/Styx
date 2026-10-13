//! The command line on capture directories written to disk: the reader's files, the report,
//! the overlay (which must parse in Styx), and the processed check.

use std::path::{Path, PathBuf};
use std::process::Command;

use styx_calibrate::synth::{self, Spec};

fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes a dark capture as the board's dump: raw samples, frames.csv and meta.txt.
fn write_capture(root: &Path, name: &str, spec: &Spec) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let cap = synth::capture(spec);
    let mut bytes = Vec::new();
    for f in 0..cap.frames {
        for y in 0..cap.height {
            for x in 0..cap.width {
                bytes.extend_from_slice(&cap.sample(f, x, y).to_le_bytes());
            }
        }
    }
    let (w, h) = spec.size;
    std::fs::write(dir.join(format!("frames_{w}x{h}_bggr16.raw")), bytes).unwrap();
    let mut csv =
        String::from("index,sensor_sequence,exposure_us,analog_gain,digital_gain,temperature\n");
    for f in 0..spec.frames {
        csv += &format!(
            "{f},{f},{},{:.4},1.0000,{:.1}\n",
            spec.exposure_us,
            spec.analog_gain,
            40.0 + 0.1 * f as f64
        );
    }
    std::fs::write(dir.join("frames.csv"), csv).unwrap();
    std::fs::write(
        dir.join("meta.txt"),
        format!(
            "width={w}\nheight={h}\ncfa=BGGR\nbit_depth=10\ncamera=ov9782 (kernel driver i2c 10-0060)\n"
        ),
    )
    .unwrap();
    dir
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_styx-calibrate"))
}

#[test]
fn dark_writes_a_report_and_an_overlay_that_styx_reads() {
    let root = scratch("dark");
    let mut a = Spec::base();
    a.black = [64.0, 66.0, 65.0, 63.0];
    a.hot = vec![(12, 14, 250.0)];
    let mut b = Spec::base();
    b.black = a.black;
    b.analog_gain = 4.0;
    b.seed = 9;
    let da = write_capture(&root, "e8000_g1.0", &a);
    let db = write_capture(&root, "e8000_g4.0", &b);
    let out = root.join("out");
    let status = bin()
        .arg("dark")
        .arg(&da)
        .arg(&db)
        .arg("--out")
        .arg(&out)
        .args([
            "--styx-commit",
            "abc123",
            "--date",
            "2026-10-12",
            "--unit",
            "raze-03",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let report = std::fs::read_to_string(out.join("calibration.md")).unwrap();
    assert!(report.contains("# Sensor calibration: ov9782"), "{report}");
    assert!(report.contains("Styx commit `abc123`"));
    // (12, 14) is an even row and column: blue in BGGR.
    assert!(report.contains("| 12 | 14 | B | hot |"), "{report}");
    let overlay = std::fs::read_to_string(out.join("ov9782.overlay.json")).unwrap();
    let t = styx_algo::Tuning::from_rpi_json_str_with_overlay(
        styx_calibrate::overlay::VALIDATION_BASE,
        &overlay,
    )
    .expect("styx reads the overlay")
    .tuning;
    let black = t.black_level.expect("black level");
    // The default is one level for the four channels (mean 64.5), and no noise profile.
    assert!((black.g - 64.5 / 1024.0).abs() < 1e-3, "g {}", black.g);
    assert_eq!(black.gb, None, "greens are not split by default");
    assert_eq!(black.by_gain.len(), 0, "the level holds across gains");
    let json: serde_json::Value = serde_json::from_str(&overlay).unwrap();
    assert_eq!(json["unit"], "raze-03");
    assert_eq!(json["defect_pixels"], serde_json::json!([[12, 14]]));
}

#[test]
fn dark_rejects_a_directory_without_a_raw_dump() {
    let root = scratch("missing");
    let out = bin().arg("dark").arg(&root).output().unwrap();
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("no raw dump"), "{text}");
}

#[test]
fn check_processed_reports_the_clipped_fraction() {
    let root = scratch("processed");
    let (w, h) = (4usize, 2usize);
    // Two frames: pixel 0 is always 0; the others 5 and 9.
    let mut bytes = Vec::new();
    for f in 0..2u8 {
        for i in 0..w * h {
            bytes.push(if i == 0 {
                0
            } else {
                5 + 4 * (f * (i as u8 % 2))
            });
        }
    }
    std::fs::write(root.join("frames_4x2_gray.raw"), bytes).unwrap();
    let out = bin().arg("check-processed").arg(&root).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    // 2 of 16 samples are 0: 12.50 %.
    assert!(text.contains("12.50 %"), "{text}");
}
