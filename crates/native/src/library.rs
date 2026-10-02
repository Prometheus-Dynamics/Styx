//! Where sensor descriptions come from: a search path of TOML files and descriptions embedded in
//! the program.
//!
//! A bridge names its sensor (`styx,sensor-name`, e.g. `ov9782`); the library looks for
//! `<name>.toml` in each directory of the search path, then for a file entry whose description
//! has that name, then among the embedded descriptions. The first match wins.
//!
//! The default search path is `$STYX_SENSOR_PATH` (colon separated; directories or files),
//! then `$XDG_CONFIG_HOME/styx/sensors` (or `~/.config/styx/sensors`), `/etc/styx/sensors`,
//! `/usr/local/share/styx/sensors` and `/usr/share/styx/sensors`.
//!
//! The descriptions built into `styx-sensor` ([`styx_sensor::BUILTIN_DESCRIPTIONS`], today the
//! OV9782) are embedded and come last, so a file of the same name on the search path overrides
//! them. Programs can embed their own with [`SensorLibrary::with_embedded`].

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use styx_sensor::{KernelSensorData, SensorDescription};

use crate::error::{NativeError, Result};

/// File name ending of the data files for sensors with kernel drivers
/// ([`SensorLibrary::find_kernel_data`]).
pub const KERNEL_DATA_SUFFIX: &str = ".kernel.toml";

/// The environment variable holding extra search path entries.
pub const SENSOR_PATH_ENV: &str = "STYX_SENSOR_PATH";

/// Descriptions compiled into this crate: `(sensor name, TOML)`, the ones that ship with
/// `styx-sensor`.
pub const EMBEDDED: &[(&str, &str)] = styx_sensor::BUILTIN_DESCRIPTIONS;

/// A search path for sensor descriptions plus embedded ones.
#[derive(Clone, Debug, Default)]
pub struct SensorLibrary {
    paths: Vec<PathBuf>,
    embedded: Vec<(String, Cow<'static, str>)>,
}

impl SensorLibrary {
    /// An empty library (nothing found until paths or descriptions are added).
    pub fn new() -> Self {
        Self::default()
    }

    /// The default search path (see the module documentation) and the embedded descriptions.
    pub fn system() -> Self {
        let mut lib = Self::new();
        if let Some(extra) = std::env::var_os(SENSOR_PATH_ENV) {
            lib.paths.extend(std::env::split_paths(&extra));
        }
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
        if let Some(c) = config {
            lib.paths.push(c.join("styx/sensors"));
        }
        for dir in [
            "/etc/styx/sensors",
            "/usr/local/share/styx/sensors",
            "/usr/share/styx/sensors",
        ] {
            lib.paths.push(PathBuf::from(dir));
        }
        for (name, toml) in EMBEDDED {
            lib.embedded
                .push(((*name).to_owned(), Cow::Borrowed(*toml)));
        }
        lib
    }

    /// Adds a directory (searched for `<name>.toml`) or a description file, searched before the
    /// entries already present.
    pub fn with_path_first(mut self, path: impl Into<PathBuf>) -> Self {
        self.paths.insert(0, path.into());
        self
    }

    /// Adds a directory or description file at the end of the search path.
    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.paths.push(path.into());
        self
    }

    /// Adds an embedded description for `name`.
    pub fn with_embedded(
        mut self,
        name: impl Into<String>,
        toml: impl Into<Cow<'static, str>>,
    ) -> Self {
        self.embedded.push((name.into(), toml.into()));
        self
    }

    /// The search path, in order.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// The names of the embedded descriptions.
    pub fn embedded_names(&self) -> impl Iterator<Item = &str> {
        self.embedded.iter().map(|(n, _)| n.as_str())
    }

    /// Finds and parses the description of sensor `name`.
    pub fn find(&self, name: &str) -> Result<Arc<SensorDescription>> {
        self.find_with_source(name).map(|(d, _)| d)
    }

    /// Like [`Self::find`], also saying where the description came from (a path, or
    /// `embedded:<name>`).
    pub fn find_with_source(&self, name: &str) -> Result<(Arc<SensorDescription>, String)> {
        if name.is_empty() || name.contains('/') {
            return Err(NativeError::InvalidConfig(format!(
                "invalid sensor name {name:?}"
            )));
        }
        let file_name = format!("{name}.toml");
        for entry in &self.paths {
            if entry.is_dir() {
                let candidate = entry.join(&file_name);
                if candidate.is_file() {
                    return load(&candidate).map(|d| (d, candidate.display().to_string()));
                }
            } else if entry.is_file() {
                // A file entry is used when its description names this sensor.
                if let Ok(d) = load(entry)
                    && d.sensor.name == name
                {
                    return Ok((d, entry.display().to_string()));
                }
            }
        }
        for (n, toml) in &self.embedded {
            if n == name {
                let source = format!("embedded:{n}");
                let desc = SensorDescription::from_toml_str(toml, &source)?;
                return Ok((Arc::new(desc), source));
            }
        }
        Err(NativeError::NoDescription(
            name.to_owned(),
            self.describe_search(),
        ))
    }

    /// The data file for a sensor a kernel driver owns, named by its media entity (e.g.
    /// `ov9782 10-0060`) and reporting colour (`true`) or mono codes: the first
    /// `*.kernel.toml` in the search path that applies (directories are read in name order;
    /// files that do not parse are skipped), else the ones built into `styx-sensor`. Returns
    /// the data and where it came from (a path, or `builtin:<name>`).
    pub fn find_kernel_data(
        &self,
        entity: &str,
        colour: bool,
    ) -> Option<(KernelSensorData, String)> {
        let is_data = |p: &Path| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(KERNEL_DATA_SUFFIX))
        };
        for entry in &self.paths {
            let mut files: Vec<PathBuf> = if entry.is_dir() {
                std::fs::read_dir(entry)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| is_data(p) && p.is_file())
                    .collect()
            } else if is_data(entry) && entry.is_file() {
                vec![entry.clone()]
            } else {
                Vec::new()
            };
            files.sort();
            for f in files {
                if let Ok(d) = KernelSensorData::from_file(&f)
                    && d.matches(entity, colour)
                {
                    return Some((d, f.display().to_string()));
                }
            }
        }
        KernelSensorData::find(&KernelSensorData::builtin(), entity, colour)
            .map(|d| (d.clone(), format!("builtin:{}", d.name)))
    }

    fn describe_search(&self) -> String {
        let mut parts: Vec<String> = self.paths.iter().map(|p| p.display().to_string()).collect();
        parts.extend(self.embedded.iter().map(|(n, _)| format!("embedded:{n}")));
        if parts.is_empty() {
            "nothing".into()
        } else {
            parts.join(", ")
        }
    }
}

fn load(path: &Path) -> Result<Arc<SensorDescription>> {
    Ok(Arc::new(SensorDescription::from_file(path)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../sensor/sensors/ov9782.toml");

    fn scratch(tag: &str) -> PathBuf {
        let dir = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/test-scratch"
        ))
        .join(format!("styx-native-lib-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_a_description_in_a_directory_by_name() {
        let dir = scratch("dir");
        std::fs::copy(FIXTURE, dir.join("ov9782.toml")).unwrap();
        let lib = SensorLibrary::new().with_path(&dir);
        let (d, source) = lib.find_with_source("ov9782").unwrap();
        assert_eq!(d.sensor.name, "ov9782");
        assert!(source.ends_with("ov9782.toml"));
        assert!(matches!(
            lib.find("imx219"),
            Err(NativeError::NoDescription(..))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_entries_match_on_the_description_name() {
        let lib = SensorLibrary::new().with_path(FIXTURE);
        assert!(lib.find("ov9782").is_ok());
        assert!(lib.find("ov9281").is_err());
    }

    #[test]
    fn embedded_descriptions_come_after_the_search_path() {
        let toml = std::fs::read_to_string(FIXTURE).unwrap();
        let lib = SensorLibrary::new().with_embedded("ov9782", toml);
        let (_, source) = lib.find_with_source("ov9782").unwrap();
        assert_eq!(source, "embedded:ov9782");
        let lib = lib.with_path_first(FIXTURE);
        let (_, source) = lib.find_with_source("ov9782").unwrap();
        assert!(source.ends_with("ov9782.toml"));
        assert_eq!(lib.embedded_names().collect::<Vec<_>>(), ["ov9782"]);
    }

    #[test]
    fn rejects_path_like_names_and_reports_the_search() {
        let lib = SensorLibrary::new().with_path("/nonexistent/styx");
        assert!(matches!(
            lib.find("../etc/passwd"),
            Err(NativeError::InvalidConfig(_))
        ));
        let err = lib.find("x").unwrap_err().to_string();
        assert!(err.contains("/nonexistent/styx"), "{err}");
        assert!(
            SensorLibrary::new()
                .find("x")
                .unwrap_err()
                .to_string()
                .contains("nothing")
        );
    }

    #[test]
    fn kernel_data_comes_from_the_search_path_then_the_builtin_files() {
        let lib = SensorLibrary::new();
        let (d, source) = lib.find_kernel_data("imx219 10-0010", true).unwrap();
        assert_eq!(
            (d.name.as_str(), source.as_str()),
            ("imx219", "builtin:imx219")
        );
        assert!(lib.find_kernel_data("imx999 10-0010", true).is_none());
        let dir = scratch("kernel");
        std::fs::write(
            dir.join("mine.kernel.toml"),
            "name = \"imx219\"\ntuning = \"mine.json\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("broken.kernel.toml"), "name = 3").unwrap();
        let lib = SensorLibrary::new().with_path(&dir);
        let (d, source) = lib.find_kernel_data("imx219 10-0010", true).unwrap();
        assert_eq!(d.tuning.as_deref(), Some("mine.json"));
        assert!(source.ends_with("mine.kernel.toml"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_system_library_embeds_the_builtin_descriptions() {
        let lib = SensorLibrary::system();
        assert_eq!(lib.embedded_names().count(), EMBEDDED.len());
        assert!(lib.embedded_names().any(|n| n == "ov9782"));
        let (d, source) = SensorLibrary::new()
            .with_path("/nonexistent/styx")
            .with_embedded("ov9782", EMBEDDED[0].1)
            .find_with_source("ov9782")
            .unwrap();
        assert_eq!(
            (d.sensor.name.as_str(), source.as_str()),
            ("ov9782", "embedded:ov9782")
        );
        assert!(
            lib.paths()
                .iter()
                .any(|p| p == Path::new("/etc/styx/sensors"))
        );
    }
}
