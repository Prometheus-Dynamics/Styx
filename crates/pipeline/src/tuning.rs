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
//! Built in:
//!
//! * `ov9782.json`: the HeliOS OV9782 tuning for the PiSP (HeliOS
//!   `gaia/assets/hardware/runtime/usr/share/libcamera/ipa/rpi/pisp/ov9782.json`, now shipped by
//!   the Atlas Raze device package), written by the project owner, who cleared its use in Styx;
//!   unchanged (md5 `f06d0ac91ed7c5f96b436d8912bd6daf`).
//! * `generic.toml`: grey world, centre weighted, default tone curve, spatial and colour
//!   denoise with the Raspberry Pi IPA's assumptions for an unknown sensor.

use std::path::{Path, PathBuf};

use styx_algo::Tuning;

/// Environment variable naming a tuning file to use instead of searching.
pub const TUNING_ENV: &str = "STYX_TUNING";

/// Environment variable holding extra tuning directories, searched first.
pub const TUNING_PATH_ENV: &str = "STYX_TUNING_PATH";

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

/// The generic tuning (styx-algo TOML), for sensors without one of their own.
pub const GENERIC_TUNING: &str = include_str!("../tuning/generic.toml");

/// Parses tuning text: `.json` names a Raspberry Pi file, anything else styx-algo's TOML.
pub fn parse_tuning(name: &str, text: &str) -> styx_algo::Result<Tuning> {
    if Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
    {
        Ok(Tuning::from_rpi_json_str(text)?.tuning)
    } else {
        Tuning::from_toml_str(text)
    }
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
        }
    }

    /// The tuning named `name` (a sensor description's `tuning`), and where it came from (a
    /// path, `builtin:<name>`, `builtin:generic` or `defaults`). Files that do not parse are
    /// skipped.
    pub fn find(&self, name: Option<&str>) -> (Tuning, String) {
        let load = |p: &Path| p.is_file().then(|| Tuning::load(p).ok()).flatten();
        if let Some(p) = &self.file
            && let Some(t) = load(p)
        {
            return (t, p.display().to_string());
        }
        // A plain file name only: a description cannot point outside the search path.
        if let Some(name) = name.filter(|n| !n.is_empty() && !n.contains('/')) {
            let own = Path::new(name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("json"))
                .then(|| Path::new(name).with_extension("toml"));
            for dir in &self.dirs {
                for p in own.iter().map(|o| dir.join(o)).chain([dir.join(name)]) {
                    if let Some(t) = load(&p) {
                        return (t, p.display().to_string());
                    }
                }
            }
            if self.builtin
                && let Some((n, text)) = BUILTIN_TUNINGS.iter().find(|(n, _)| *n == name)
                && let Ok(t) = parse_tuning(n, text)
            {
                return (t, format!("builtin:{n}"));
            }
            for dir in &self.fallback_dirs {
                let p = dir.join(name);
                if let Some(t) = load(&p) {
                    return (t, p.display().to_string());
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
}

/// The tuning for a sensor description, from [`TuningLibrary::system`].
pub fn find_tuning(desc: &styx_sensor::SensorDescription) -> (Tuning, String) {
    TuningLibrary::system().find(desc.sensor.tuning.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/test-scratch"
        ))
        .join(format!("styx-tuning-{tag}-{}", std::process::id()));
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

    #[test]
    fn search_order_is_files_then_builtin_then_fallback_then_generic() {
        let (_, src) = lib().find(Some("ov9782.json"));
        assert_eq!(src, "builtin:ov9782.json");
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
        assert_eq!(l.find(Some("ov9782.json")).1, "builtin:ov9782.json");
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
}
