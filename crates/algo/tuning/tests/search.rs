//! The search order, the override, the fallback and what the result says.

use std::fs;
use std::path::{Path, PathBuf};

use styx_tuning::{GENERIC_NAME, Origin, Source, TuningSearch};

fn data() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// The bundled install directory of this checkout (`data/tuning`).
fn bundled_install() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../data/tuning")
}

/// A scratch directory of this test binary, unique per test.
fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("tuning-{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    fs::write(&p, text).unwrap();
    p
}

#[test]
fn a_named_file_in_the_bundled_directory_is_used_and_says_so() {
    let root = scratch("bundled");
    let bundled = root.join("bundled");
    write(&bundled, "cam.toml", "description = \"bundled\"\n");
    let found = TuningSearch::new([], Some(bundled.clone())).find(Some("cam.toml"));
    assert_eq!(found.tuning.description.as_deref(), Some("bundled"));
    assert_eq!(
        found.source,
        Source::File {
            path: bundled.join("cam.toml"),
            origin: Origin::Bundled
        }
    );
    assert!(!found.is_fallback());
    assert!(found.skipped.is_empty());
    assert!(
        found.describe().contains("cam.toml (bundled)"),
        "{}",
        found.describe()
    );
}

#[test]
fn an_added_file_overrides_a_bundled_one_of_the_same_name() {
    let root = scratch("override");
    let added = root.join("added");
    let bundled = root.join("bundled");
    write(&bundled, "cam.toml", "description = \"bundled\"\n");
    write(&added, "cam.toml", "description = \"added\"\n");
    let found = TuningSearch::new([added.clone()], Some(bundled)).find(Some("cam.toml"));
    assert_eq!(found.tuning.description.as_deref(), Some("added"));
    assert_eq!(
        found.source,
        Source::File {
            path: added.join("cam.toml"),
            origin: Origin::Added
        }
    );
}

#[test]
fn added_directories_are_searched_in_order() {
    let root = scratch("order");
    let first = root.join("first");
    let second = root.join("second");
    write(&first, "cam.toml", "description = \"first\"\n");
    write(&second, "cam.toml", "description = \"second\"\n");
    let found = TuningSearch::new([second.clone(), first], None).find(Some("cam.toml"));
    assert_eq!(found.tuning.description.as_deref(), Some("second"));
}

#[test]
fn a_file_that_does_not_load_is_skipped_with_its_reason_and_the_next_one_wins() {
    let root = scratch("broken");
    let added = root.join("added");
    let bundled = root.join("bundled");
    write(&added, "cam.toml", "this is = = not toml\n");
    write(&bundled, "cam.toml", "description = \"bundled\"\n");
    let found = TuningSearch::new([added.clone()], Some(bundled.clone())).find(Some("cam.toml"));
    assert_eq!(found.tuning.description.as_deref(), Some("bundled"));
    assert_eq!(found.skipped.len(), 1);
    assert_eq!(found.skipped[0].path, added.join("cam.toml"));
    assert!(
        found.skipped[0].reason.contains("not styx-aaa TOML"),
        "{:?}",
        found.skipped
    );
    assert!(found.describe().contains("skipped"), "{}", found.describe());
}

#[test]
fn an_unknown_key_is_refused_as_not_styx_aaa_toml() {
    let found = TuningSearch::new([data()], None).find(Some("fixturecam-unknown-key.toml"));
    assert!(found.is_fallback());
    assert!(
        found.skipped[0].reason.contains("not styx-aaa TOML"),
        "{:?}",
        found.skipped
    );
}

#[test]
fn a_missing_tuning_falls_back_to_the_generic_one_and_says_so() {
    let found = TuningSearch::new([data()], Some(bundled_install())).find(Some("nosuchcam.toml"));
    assert_eq!(
        found.source,
        Source::Generic {
            path: bundled_install().join(GENERIC_NAME),
            origin: Origin::Bundled
        }
    );
    assert!(found.is_fallback());
    assert_eq!(found.requested.as_deref(), Some("nosuchcam.toml"));
    let line = found.describe();
    assert!(
        line.contains("no tuning named nosuchcam.toml found"),
        "{line}"
    );
    assert!(line.contains(GENERIC_NAME), "{line}");
}

#[test]
fn no_name_gives_the_generic_tuning() {
    let found = TuningSearch::new([], Some(bundled_install())).find(None);
    assert!(matches!(found.source, Source::Generic { .. }));
    assert_eq!(found.requested, None);
    assert!(
        found.describe().contains("(none named)"),
        "{}",
        found.describe()
    );
}

#[test]
fn with_no_generic_file_the_defaults_are_used_and_the_result_says_so() {
    let root = scratch("defaults");
    let empty = root.join("empty");
    fs::create_dir_all(&empty).unwrap();
    let found = TuningSearch::new([empty], None).find(Some("nosuchcam.toml"));
    assert_eq!(found.source, Source::Defaults);
    assert_eq!(found.tuning, styx_aaa::Tuning::default());
    assert!(
        found.describe().contains("defaults"),
        "{}",
        found.describe()
    );
}

#[test]
fn a_name_with_a_directory_part_is_refused() {
    let found =
        TuningSearch::new([data()], Some(bundled_install())).find(Some("../fixturecam.toml"));
    assert!(matches!(found.source, Source::Generic { .. }));
    assert_eq!(found.skipped.len(), 1);
    assert!(found.skipped[0].reason.contains("not a plain file name"));
    let found = TuningSearch::new([data()], None).find(Some("sub/fixturecam.toml"));
    assert!(found.is_fallback());
}

#[test]
fn a_json_name_looks_for_the_toml_of_the_same_stem_first() {
    let root = scratch("json-stem");
    let added = root.join("added");
    write(&added, "cam.toml", "description = \"styx toml\"\n");
    write(
        &added,
        "cam.json",
        "{\"version\": 2.0, \"algorithms\": []}\n",
    );
    let found = TuningSearch::new([added.clone()], None).find(Some("cam.json"));
    assert_eq!(found.tuning.description.as_deref(), Some("styx toml"));
    assert!(
        matches!(found.source, Source::File { ref path, .. } if path == &added.join("cam.toml"))
    );
}

#[test]
fn a_json_tuning_alone_is_skipped_with_the_reason_not_read() {
    let root = scratch("json-only");
    let added = root.join("added");
    write(
        &added,
        "cam.json",
        "{\"version\": 2.0, \"algorithms\": []}\n",
    );
    let found = TuningSearch::new([added], Some(bundled_install())).find(Some("cam.json"));
    assert!(matches!(found.source, Source::Generic { .. }));
    assert_eq!(found.skipped.len(), 1);
    assert!(
        found.skipped[0].reason.contains("Raspberry Pi JSON"),
        "{:?}",
        found.skipped
    );
}

#[test]
fn the_installed_data_loads_its_generic_tuning() {
    let found = TuningSearch::new([], Some(bundled_install())).find(None);
    let denoise = found
        .tuning
        .denoise
        .as_ref()
        .expect("generic has a denoise section");
    assert!(denoise.validate().is_ok());
    assert!(matches!(
        found.source,
        Source::Generic {
            origin: Origin::Bundled,
            ..
        }
    ));
}

#[test]
fn the_module_tuning_in_the_install_is_found_but_not_read_yet() {
    let found = TuningSearch::new([], Some(bundled_install())).find(Some("ov9782.json"));
    assert!(
        matches!(found.source, Source::Generic { .. }),
        "{:?}",
        found.source
    );
    assert_eq!(found.skipped.len(), 1);
    assert!(
        found.describe().contains("ov9782.json"),
        "{}",
        found.describe()
    );
}

#[test]
fn a_fixture_tuning_of_a_sensor_is_found_by_name() {
    let found = TuningSearch::new([data()], None).find(Some("fixturecam.toml"));
    assert_eq!(
        found.tuning.description.as_deref(),
        Some("fixture tuning (test data)")
    );
    assert_eq!(found.tuning.denoise.unwrap().dpc, 2);
}

#[test]
fn system_searches_the_board_directory_then_the_bundled_install() {
    let s = TuningSearch::system();
    assert_eq!(
        s.added.last().map(PathBuf::as_path),
        Some(Path::new("/etc/styx/tuning"))
    );
    assert_eq!(
        s.bundled.as_deref(),
        Some(Path::new("/usr/share/styx/tuning"))
    );
}
