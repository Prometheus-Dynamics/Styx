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
//! No description ships embedded yet: the only one written so far (OV9782) carries register
//! values derived from a GPL driver and is test data until it is rewritten from the datasheet,
//! so it is loaded from a file. Programs can embed their own with
//! [`SensorLibrary::with_embedded`].

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use styx_sensor::SensorDescription;

use crate::error::{NativeError, Result};

/// The environment variable holding extra search path entries.
pub const SENSOR_PATH_ENV: &str = "STYX_SENSOR_PATH";

/// Descriptions compiled into this crate: `(sensor name, TOML)`. Empty on purpose (see the
/// module documentation).
pub const EMBEDDED: &[(&str, &str)] = &[];

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
    fn the_system_library_embeds_nothing_and_honours_the_environment() {
        let lib = SensorLibrary::system();
        assert_eq!(lib.embedded_names().count(), EMBEDDED.len());
        assert!(
            lib.paths()
                .iter()
                .any(|p| p == Path::new("/etc/styx/sensors"))
        );
    }
}
