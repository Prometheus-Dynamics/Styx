//! The `styx-record` command: `info` on a finished, an unfinished and a truncated recording,
//! `export` then `import` with the counts and losses printed, and the refusals (bad arguments,
//! a file that is not a recording).

use crate::mcap_export::recording;
use crate::support::samples;

use std::path::PathBuf;
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_styx-record"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

/// A fresh directory for this test under the target's temporary directory.
fn dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn line<'a>(text: &'a str, key: &str) -> &'a str {
    text.lines()
        .find(|l| l.trim_start().starts_with(key))
        .unwrap_or_else(|| panic!("no {key:?} line in\n{text}"))
}

#[test]
fn info_export_import_round_trip() {
    let d = dir("cli-round-trip");
    let frames = samples(61);
    let (bytes, written) = recording(&frames);
    let rec = d.join("a.styxrec");
    std::fs::write(&rec, &bytes).unwrap();
    let rec = rec.to_str().unwrap();

    let info = stdout(&run(&["info", rec]));
    assert!(line(&info, "device").contains("export camera"));
    assert!(line(&info, "keys").contains("serial-42"));
    assert_eq!(line(&info, "frames"), format!("frames    {}", frames.len()));
    assert!(line(&info, "channels").ends_with(" 3"));
    let imu_samples = written.iter().filter(|w| w.0 == 0).count();
    let imu = line(&info, "0 /helios/imu");
    assert!(imu.contains("clock=boottime"), "{imu}");
    assert!(imu.contains(&format!("samples={imu_samples}")), "{imu}");
    assert!(imu.contains("declared mono=900 boot=1900"), "{imu}");
    assert!(line(&info, "source=").contains("lemnosd"));
    assert!(line(&info, "2 /late").contains("samples=0"));
    assert!(line(&info, "end").contains("clean"));

    let mcap = d.join("a.mcap");
    let mcap = mcap.to_str().unwrap();
    let out = stdout(&run(&["export", rec, mcap, "--chunk-size", "4096"]));
    assert!(
        out.contains(&format!(
            "{} frames, 3 channels, {} samples (0 late), source complete",
            frames.len(),
            written.len()
        )),
        "{out}"
    );
    assert!(out.contains("not carried into MCAP"));
    // The fixture: every frame has manual controls, the grey frame has padded rows, sensor
    // times equal frame times, ids fit u32, lux and colour temperature are never zero.
    for (key, n) in [
        ("control values", 4),
        ("row/plane padding", 1),
        ("ids above u32", 0),
        ("sensor timestamps", 0),
        ("zero lux or colour temperature", 0),
    ] {
        assert!(line(&out, key).ends_with(&format!(" {n}")), "{key}: {out}");
    }

    let back = d.join("b.styxrec");
    let back = back.to_str().unwrap();
    let out = stdout(&run(&["import", mcap, back]));
    assert!(
        out.contains(&format!(
            "{} frames, 3 channels, {} samples, 3 clock records, source complete",
            frames.len(),
            written.len()
        )),
        "{out}"
    );
    assert!(line(&out, "boottime as monotonic").ends_with(" 0"));
    // The export's sequences are the per-channel counts the import expects.
    assert!(
        line(&out, "data message sequences").ends_with(" 0"),
        "{out}"
    );

    let info = stdout(&run(&["info", back]));
    assert_eq!(line(&info, "frames"), format!("frames    {}", frames.len()));
    assert!(line(&info, "0 /helios/imu").contains(&format!("samples={imu_samples}")));
    assert!(line(&info, "end").contains("clean"));

    // Uncompressed too.
    let raw = d.join("raw.mcap");
    stdout(&run(&[
        "export",
        rec,
        raw.to_str().unwrap(),
        "--uncompressed",
    ]));
    if cfg!(feature = "mcap-lz4") {
        assert!(std::fs::metadata(&raw).unwrap().len() > std::fs::metadata(mcap).unwrap().len());
    }
}

#[test]
fn info_tells_finished_unfinished_and_truncated_apart() {
    let d = dir("cli-endings");
    let frames = samples(62);
    let (bytes, _) = recording(&frames);

    // Without the end marker (the last record) but whole: not finished. The end marker is the
    // file's last byte (a bare tag).
    let unfinished = d.join("unfinished.styxrec");
    std::fs::write(&unfinished, &bytes[..bytes.len() - 1]).unwrap();
    let info = stdout(&run(&["info", unfinished.to_str().unwrap()]));
    assert!(line(&info, "end").contains("not finished"), "{info}");

    // Cut inside the last frame: truncated, and fewer frames.
    let cut = d.join("cut.styxrec");
    let at = bytes.len() * 3 / 4;
    std::fs::write(&cut, &bytes[..at]).unwrap();
    let info = stdout(&run(&["info", cut.to_str().unwrap()]));
    assert!(line(&info, "end").contains("truncated"), "{info}");
}

#[test]
fn refusals() {
    let d = dir("cli-refusals");
    for args in [
        &[][..],
        &["info"],
        &["export", "a"],
        &["export", "a", "b", "--chunk-size"],
        &["export", "a", "b", "--fast"],
        &["frobnicate", "a"],
    ] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
    }
    let junk = d.join("junk.styxrec");
    std::fs::write(&junk, b"not a recording at all").unwrap();
    let out = run(&["info", junk.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("styx-record: "));
    let out = run(&["info", d.join("missing").to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
}
