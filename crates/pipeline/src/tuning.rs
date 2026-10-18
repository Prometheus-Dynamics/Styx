//! Where a sensor's tuning comes from.
//!
//! A sensor description names its tuning file (`tuning = "ov9782.json"`). It is looked up, the
//! first match winning, in:
//!
//! 1. [`TUNING_ENV`] (`STYX_TUNING`): one file, used for every sensor;
//! 2. [`TUNING_PATH_ENV`] (`STYX_TUNING_PATH`, colon separated directories), then
//!    `$XDG_CONFIG_HOME/styx/tuning` (or `~/.config/styx/tuning`), `/etc/styx/tuning`,
//!    `/usr/local/share/styx/tuning` and `/usr/share/styx/tuning`;
//! 3. the tunings built into this crate ([`BUILTIN_TUNINGS`]);
//! 4. libcamera's Raspberry Pi directories ([`LIBCAMERA_TUNING_DIRS`], read at run time, never
//!    copied), for sensors Styx has no tuning of its own for;
//! 5. Styx's generic tuning ([`GENERIC_TUNING`]), else styx-algo's defaults.
//!
//! Files ending in `.json` are Raspberry Pi tuning files, anything else styx-algo's TOML. In
//! Styx's own directories a `.json` name is looked for as `<stem>.toml` first (what
//! `styx-tune` writes, with the Styx-only settings the JSON cannot hold), then as named.
//!
//! # Overlays
//!
//! A tuning overlay is a JSON file of calibrated settings (written by `styx-calibrate`) merged
//! over a Raspberry Pi (JSON) tuning, whichever one it is: the built-in one or a file found by
//! the search above. Its algorithm blocks deep-merge into the blocks of the same name (see
//! [`styx_algo::Tuning::from_rpi_json_str_with_overlay`]); a block the tuning lacks is added.
//! The overlay is used when its `sensor` key (if it has one) names the tuning's sensor, which
//! is the file name's stem (`ov9782` for `ov9782.json`). Where it comes from, first match:
//!
//! 1. the configuration's `tuning_overlay` (a file; `StyxConfig::tuning_overlay`);
//! 2. [`OVERLAY_ENV`] (`STYX_TUNING_OVERLAY`), a file;
//! 3. `<sensor>.overlay.json` in the directories above (for example
//!    `/etc/styx/tuning/ov9782.overlay.json`);
//! 4. the built-in overlay for the sensor ([`BUILTIN_OVERLAYS`], when built-in tunings are on):
//!    the OV9782's calibrated black level per gain.
//!
//! The source string of the tuning (logged when a capture starts) names the overlay applied,
//! or says why one was not.
//!
//! Built in:
//!
//! * `ov9782.json`: the OV9782 tuning for the PiSP (the copy shipped in a CM5 image's
//!   libcamera `ipa/rpi/pisp` directory), written by the project owner, who cleared its use in Styx;
//!   unchanged (md5 `f06d0ac91ed7c5f96b436d8912bd6daf`).
//! * `generic.toml`: grey world, centre weighted, default tone curve, spatial and colour
//!   denoise with the Raspberry Pi IPA's assumptions for an unknown sensor.

use std::path::{Path, PathBuf};

use styx_algo::Tuning;
use styx_algo::tuning::json;

/// Environment variable naming a tuning file to use instead of searching.
pub const TUNING_ENV: &str = "STYX_TUNING";

/// Environment variable holding extra tuning directories, searched first.
pub const TUNING_PATH_ENV: &str = "STYX_TUNING_PATH";

/// Environment variable naming a tuning overlay file (see the [module documentation](self)).
pub const OVERLAY_ENV: &str = "STYX_TUNING_OVERLAY";

/// The name of a sensor's overlay in a tuning directory: `<sensor>.overlay.json`.
pub const OVERLAY_SUFFIX: &str = ".overlay.json";

/// Styx's own system tuning directories, after [`TUNING_PATH_ENV`] and the user's
/// configuration directory.
pub const STYX_TUNING_DIRS: &[&str] = &[
    "/etc/styx/tuning",
    "/usr/local/share/styx/tuning",
    "/usr/share/styx/tuning",
];

/// libcamera's Raspberry Pi tuning directories, the last place a named tuning is looked for.
pub const LIBCAMERA_TUNING_DIRS: &[&str] = &[
    "/usr/local/share/libcamera/ipa/rpi/pisp",
    "/usr/share/libcamera/ipa/rpi/pisp",
];

/// Tunings compiled into this crate: `(file name, contents)`.
pub const BUILTIN_TUNINGS: &[(&str, &str)] =
    &[("ov9782.json", include_str!("../tuning/ov9782.json"))];

/// Overlays compiled into this crate, used for their sensor when no overlay file is found:
/// `(file name, contents)`. The OV9782's carries its calibrated black level per gain.
pub const BUILTIN_OVERLAYS: &[(&str, &str)] = &[(
    "ov9782.overlay.json",
    include_str!("../tuning/ov9782.overlay.json"),
)];

/// The generic tuning (styx-algo TOML), for sensors without one of their own.
pub const GENERIC_TUNING: &str = include_str!("../tuning/generic.toml");

/// Parses tuning text: `.json` names a Raspberry Pi file, anything else styx-algo's TOML.
pub fn parse_tuning(name: &str, text: &str) -> styx_algo::Result<Tuning> {
    if is_json(name) {
        Ok(Tuning::from_rpi_json_str(text)?.tuning)
    } else {
        Tuning::from_toml_str(text)
    }
}

fn is_json(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
}

/// A tuning search path (see the [module documentation](self)).
#[derive(Clone, Debug, Default)]
pub struct TuningLibrary {
    /// One file used for every sensor.
    pub file: Option<PathBuf>,
    /// Styx tuning directories, in order.
    pub dirs: Vec<PathBuf>,
    /// Whether the built-in tunings are used.
    pub builtin: bool,
    /// Directories searched after the built-in tunings.
    pub fallback_dirs: Vec<PathBuf>,
    /// An overlay file, tried before the per-sensor overlays in `dirs` (see the module
    /// documentation). From the configuration or [`OVERLAY_ENV`].
    pub overlay: Option<PathBuf>,
}

impl TuningLibrary {
    /// The default search path, from the environment.
    pub fn system() -> Self {
        let mut dirs = Vec::new();
        if let Some(extra) = std::env::var_os(TUNING_PATH_ENV) {
            dirs.extend(std::env::split_paths(&extra).filter(|p| !p.as_os_str().is_empty()));
        }
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
        if let Some(c) = config {
            dirs.push(c.join("styx/tuning"));
        }
        dirs.extend(STYX_TUNING_DIRS.iter().map(PathBuf::from));
        Self {
            file: std::env::var_os(TUNING_ENV).map(PathBuf::from),
            dirs,
            builtin: true,
            fallback_dirs: LIBCAMERA_TUNING_DIRS.iter().map(PathBuf::from).collect(),
            overlay: std::env::var_os(OVERLAY_ENV).map(PathBuf::from),
        }
    }

    /// The tuning named `name` (a sensor description's `tuning`), and where it came from (a
    /// path, `builtin:<name>`, `builtin:generic` or `defaults`, with the overlay applied to it
    /// or why none was). Files that do not parse are skipped.
    pub fn find(&self, name: Option<&str>) -> (Tuning, String) {
        if let Some(p) = &self.file
            && let Some(found) = self.load(p)
        {
            return found;
        }
        // A plain file name only: a description cannot point outside the search path.
        if let Some(name) = name.filter(|n| !n.is_empty() && !n.contains('/')) {
            let own = is_json(name).then(|| Path::new(name).with_extension("toml"));
            for dir in &self.dirs {
                for p in own.iter().map(|o| dir.join(o)).chain([dir.join(name)]) {
                    if let Some(found) = self.load(&p) {
                        return found;
                    }
                }
            }
            if self.builtin
                && let Some((n, text)) = BUILTIN_TUNINGS.iter().find(|(n, _)| *n == name)
                && let Some((t, extra)) = self.parse(n, text)
            {
                return (t, format!("builtin:{n}{extra}"));
            }
            for dir in &self.fallback_dirs {
                let p = dir.join(name);
                if let Some(found) = self.load(&p) {
                    return found;
                }
            }
        }
        if self.builtin
            && let Ok(t) = Tuning::from_toml_str(GENERIC_TUNING)
        {
            return (t, "builtin:generic".into());
        }
        (Tuning::default(), "defaults".into())
    }

    /// The tuning file at `p`, if it parses: the tuning and its source (the path, and the
    /// overlay note).
    fn load(&self, p: &Path) -> Option<(Tuning, String)> {
        if !p.is_file() {
            return None;
        }
        let text = std::fs::read_to_string(p).ok()?;
        let file = p.file_name()?.to_string_lossy().into_owned();
        let (t, extra) = self.parse(&file, &text)?;
        Some((t, format!("{}{extra}", p.display())))
    }

    /// `text` (a tuning file named `file`) parsed, with the overlay for its sensor merged over
    /// it when the tuning is Raspberry Pi JSON. The note says which overlay was applied, or
    /// why one was not. `None` when the text does not parse.
    fn parse(&self, file: &str, text: &str) -> Option<(Tuning, String)> {
        let plain = || parse_tuning(file, text).ok().map(|t| (t, String::new()));
        if !is_json(file) {
            return plain();
        }
        let sensor = Path::new(file).file_stem()?.to_string_lossy().into_owned();
        let Some((path, overlay)) = self.overlay_for(&sensor) else {
            return plain();
        };
        match Tuning::from_rpi_json_str_with_overlay(text, &overlay) {
            Ok(import) => Some((import.tuning, format!(" overlay={}", path.display()))),
            Err(e) => {
                plain().map(|(t, _)| (t, format!(" overlay-rejected={}: {e}", path.display())))
            }
        }
    }

    /// The overlay for `sensor` (its path and text), from [`Self::overlay`] first, then the
    /// `<sensor>.overlay.json` files of [`Self::dirs`]. An overlay that names another sensor is
    /// skipped.
    fn overlay_for(&self, sensor: &str) -> Option<(PathBuf, String)> {
        let explicit = self.overlay.iter().cloned();
        let per_sensor = self
            .dirs
            .iter()
            .map(|d| d.join(format!("{sensor}{OVERLAY_SUFFIX}")));
        for p in explicit.chain(per_sensor) {
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            // Not JSON, or naming another sensor: not for this tuning.
            let Ok(doc) = json::parse(&text) else {
                continue;
            };
            match doc.get("sensor") {
                None => {}
                Some(v) if v.as_str() == Some(sensor) => {}
                Some(_) => continue,
            }
            return Some((p, text));
        }
        let name = format!("{sensor}{OVERLAY_SUFFIX}");
        BUILTIN_OVERLAYS
            .iter()
            .find(|(n, _)| self.builtin && *n == name)
            .map(|(n, text)| (PathBuf::from(format!("builtin:{n}")), (*text).to_owned()))
    }
}

/// The tuning for a sensor description, from [`TuningLibrary::system`].
pub fn find_tuning(desc: &styx_sensor::SensorDescription) -> (Tuning, String) {
    TuningLibrary::system().find(desc.sensor.tuning.as_deref())
}

/// [`find_tuning`] with an overlay file named by the configuration (`overlay`), which takes
/// the place of [`OVERLAY_ENV`].
pub fn find_tuning_with(
    desc: &styx_sensor::SensorDescription,
    overlay: Option<&Path>,
) -> (Tuning, String) {
    let mut lib = TuningLibrary::system();
    if let Some(o) = overlay {
        lib.overlay = Some(o.to_path_buf());
    }
    lib.find(desc.sensor.tuning.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        // The system temp dir, not the source tree (whose target/ may be on a slow disk).
        let dir = std::env::temp_dir().join(format!("styx-tuning-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn lib() -> TuningLibrary {
        TuningLibrary {
            builtin: true,
            ..TuningLibrary::default()
        }
    }

    #[test]
    fn builtin_tunings_parse() {
        for (name, text) in BUILTIN_TUNINGS {
            let t = parse_tuning(name, text).unwrap();
            assert!(
                t.agc.is_some() && t.awb.is_some() && t.ccm.is_some(),
                "{name}"
            );
        }
        let g = Tuning::from_toml_str(GENERIC_TUNING).unwrap();
        assert!(g.denoise.as_ref().is_some_and(|d| d.tdn.is_none()));
        styx_algo::Pipeline::from_tuning(&g).unwrap();
    }

    /// The built-in OV9782 tuning's source with its built-in overlay.
    const BUILTIN_OV9782: &str = "builtin:ov9782.json overlay=builtin:ov9782.overlay.json";

    #[test]
    fn the_builtin_ov9782_overlay_sets_the_calibrated_black_level_by_gain() {
        let (t, src) = lib().find(Some("ov9782.json"));
        assert_eq!(src, BUILTIN_OV9782);
        let b = t.black_level.unwrap();
        let gains: Vec<f64> = b.by_gain.iter().map(|g| g.gain).collect();
        assert_eq!(gains, [1.0, 2.0, 4.0, 7.75, 10.0, 15.88]);
        // 1x: 4172.8 of 65536 (65.2 in 10 bits), above the tuning's 64 the sensor never sits at.
        assert!((b.by_gain[0].g * 65536.0 - 4172.8).abs() < 0.01);
        assert_eq!((b.gr, b.gb), (None, None));
        // Without built-in tunings there is no built-in overlay either.
        let mut l = lib();
        l.builtin = false;
        l.fallback_dirs.clear();
        assert_eq!(l.find(Some("ov9782.json")).1, "defaults");
    }

    #[test]
    fn search_order_is_files_then_builtin_then_fallback_then_generic() {
        let (_, src) = lib().find(Some("ov9782.json"));
        assert_eq!(src, BUILTIN_OV9782);
        let (_, src) = lib().find(Some("imx219.json"));
        assert_eq!(src, "builtin:generic");
        let (_, src) = lib().find(None);
        assert_eq!(src, "builtin:generic");
        let (t, src) = TuningLibrary::default().find(Some("ov9782.json"));
        assert_eq!((t, src.as_str()), (Tuning::default(), "defaults"));

        let styx = scratch("styx");
        let fallback = scratch("fallback");
        std::fs::write(styx.join("ov9782.json"), "not json").unwrap();
        std::fs::write(fallback.join("imx219.json"), BUILTIN_TUNINGS[0].1).unwrap();
        std::fs::write(fallback.join("ov9782.json"), BUILTIN_TUNINGS[0].1).unwrap();
        let mut l = TuningLibrary {
            dirs: vec![styx.clone()],
            fallback_dirs: vec![fallback.clone()],
            ..lib()
        };
        // A broken file is skipped; the built-in one wins over the fallback directory.
        assert_eq!(l.find(Some("ov9782.json")).1, BUILTIN_OV9782);
        let (_, src) = l.find(Some("imx219.json"));
        assert!(src.starts_with(fallback.to_str().unwrap()), "{src}");
        std::fs::write(
            styx.join("ov9782.json"),
            "{\"version\": 2.0, \"algorithms\": []}",
        )
        .unwrap();
        let (_, src) = l.find(Some("ov9782.json"));
        assert!(src.starts_with(styx.to_str().unwrap()), "{src}");
        // styx-tune's TOML of the same name comes first.
        std::fs::write(styx.join("ov9782.toml"), "description = \"tuned\"\n").unwrap();
        let (t, src) = l.find(Some("ov9782.json"));
        assert!(src.ends_with("ov9782.toml"), "{src}");
        assert_eq!(t.description.as_deref(), Some("tuned"));
        assert_eq!(l.find(Some("../ov9782.json")).1, "builtin:generic");
        std::fs::write(styx.join("mine.toml"), "description = \"mine\"\n").unwrap();
        l.file = Some(styx.join("mine.toml"));
        let (t, _) = l.find(Some("ov9782.json"));
        assert_eq!(t.description.as_deref(), Some("mine"));
        std::fs::remove_dir_all(&styx).unwrap();
        std::fs::remove_dir_all(&fallback).unwrap();
    }

    /// The overlay's test blocks: a split green on the built-in ov9782.
    const SPLIT: &str = r#"{"sensor": "ov9782", "algorithms": [
        {"rpi.black_level": {"black_level_gr": 4100, "black_level_gb": 4104}}]}"#;

    #[test]
    fn an_overlay_naming_the_sensor_merges_over_the_builtin_tuning() {
        let dir = scratch("overlay-named");
        let path = dir.join("calibrated.json");
        std::fs::write(&path, SPLIT).unwrap();
        let mut l = lib();
        l.overlay = Some(path.clone());
        let (t, src) = l.find(Some("ov9782.json"));
        assert!(src.starts_with("builtin:ov9782.json overlay="), "{src}");
        assert!(src.contains(&path.display().to_string()), "{src}");
        let b = t.black_level.clone().unwrap();
        assert_eq!(b.gr, Some(4100.0 / 65536.0));
        assert_eq!(b.gb, Some(4104.0 / 65536.0));
        // The rest of the builtin tuning is there.
        assert!(t.awb.is_some() && t.ccm.is_some());
        // An overlay is for its sensor: a tuning of another name is not overlaid.
        let (_, src) = l.find(Some("imx219.json"));
        assert_eq!(src, "builtin:generic");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_overlay_for_another_sensor_or_not_json_is_skipped() {
        let dir = scratch("overlay-other");
        let other = dir.join("other.json");
        std::fs::write(&other, SPLIT.replace("ov9782", "imx219")).unwrap();
        let garbage = dir.join("garbage.json");
        std::fs::write(&garbage, "{not json").unwrap();
        for path in [other, garbage] {
            let mut l = lib();
            l.overlay = Some(path);
            let (t, src) = l.find(Some("ov9782.json"));
            // Skipped: the built-in overlay applies instead (uniform black, no split green).
            assert_eq!(src, BUILTIN_OV9782);
            assert_eq!(t.black_level.unwrap().gr, None);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sensor_overlay_in_a_tuning_directory_is_found_by_name() {
        let dir = scratch("overlay-dir");
        std::fs::write(
            dir.join("ov9782.overlay.json"),
            r#"{"algorithms": [{"rpi.black_level": {"black_level_gr": 4100}}]}"#,
        )
        .unwrap();
        let mut l = lib();
        l.dirs = vec![dir.clone()];
        let (t, src) = l.find(Some("ov9782.json"));
        assert!(src.starts_with("builtin:ov9782.json overlay="), "{src}");
        assert_eq!(t.black_level.unwrap().gr, Some(4100.0 / 65536.0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_rejected_overlay_leaves_the_tuning_and_says_so() {
        let dir = scratch("overlay-rejected");
        let path = dir.join("bad.json");
        std::fs::write(
            &path,
            r#"{"sensor": "ov9782", "algorithms": [{"rpi.black_level": {"black_level": 70000}}]}"#,
        )
        .unwrap();
        let mut l = lib();
        l.overlay = Some(path);
        let (t, src) = l.find(Some("ov9782.json"));
        assert!(
            src.starts_with("builtin:ov9782.json overlay-rejected="),
            "{src}"
        );
        let builtin = parse_tuning("ov9782.json", BUILTIN_TUNINGS[0].1).unwrap();
        assert_eq!(t, builtin);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
