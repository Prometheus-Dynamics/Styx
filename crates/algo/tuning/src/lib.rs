//! Finding a camera's tuning file.
//!
//! A sensor description names its tuning (a plain file name such as `<sensor>.toml`). [`TuningSearch::find`]
//! looks for it in order:
//!
//! 1. the added directories, in order (`STYX_TUNING_PATH`, colon separated, then `/etc/styx/tuning`
//!    in [`TuningSearch::system`]). An added file wins over a bundled one of the same name;
//! 2. the bundled directory (`/usr/share/styx/tuning` in [`TuningSearch::system`], where Styx's
//!    install puts `data/tuning`);
//! 3. the generic tuning ([`GENERIC_NAME`]), searched the same way;
//! 4. styx-aaa's defaults, when there is no generic file either.
//!
//! The first file that loads wins. A file that is missing is passed over silently; one that does
//! not load (not a file, not readable, not TOML of styx-aaa's format, failing validation) is passed
//! over and recorded in [`Found::skipped`] with the reason. A name with a directory part is refused:
//! a sensor description cannot point outside the search path.
//!
//! A name ending in `.json` (a Raspberry Pi tuning file) looks first for the same stem with `.toml`,
//! the form Styx's own tooling writes, and then for the name as given. A Raspberry Pi JSON file is
//! not read by this crate yet, so it is skipped with that reason.
//!
//! [`Found`] says which file was used ([`Found::source`], [`Found::describe`]); the caller logs it.

use std::path::{Path, PathBuf};

use styx_aaa::Tuning;

/// Environment variable holding added tuning directories, colon separated, searched first.
pub const TUNING_PATH_ENV: &str = "STYX_TUNING_PATH";

/// The directory a board build adds tunings to, searched after [`TUNING_PATH_ENV`].
pub const ADDED_DIR: &str = "/etc/styx/tuning";

/// Where Styx's install puts the tunings in `data/tuning`.
pub const BUNDLED_DIR: &str = "/usr/share/styx/tuning";

/// The generic tuning's file name, for a sensor with no tuning of its own.
pub const GENERIC_NAME: &str = "generic.toml";

/// Whether a directory is added by the configuration or the board, or is the bundled install.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// An added directory (a board build, the environment, a configured path).
    Added,
    /// The bundled install directory.
    Bundled,
}

impl Origin {
    fn word(self) -> &'static str {
        match self {
            Origin::Added => "added",
            Origin::Bundled => "bundled",
        }
    }
}

/// The file a [`Found`] tuning came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The named tuning file.
    File { path: PathBuf, origin: Origin },
    /// The generic tuning, because the named one was not found (or none was named).
    Generic { path: PathBuf, origin: Origin },
    /// No file at all: styx-aaa's defaults.
    Defaults,
}

/// A search path: added directories, then the bundled one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuningSearch {
    /// Added directories, searched in order.
    pub added: Vec<PathBuf>,
    /// The bundled directory, searched after the added ones (none for a search with no install).
    pub bundled: Option<PathBuf>,
}

/// A file that was passed over, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    /// The path that was tried.
    pub path: PathBuf,
    /// Why it did not load.
    pub reason: String,
}

/// The result of a search: the tuning, which file it came from, and the files passed over.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    /// The tuning to run with.
    pub tuning: Tuning,
    /// Where it came from.
    pub source: Source,
    /// The name the sensor asked for, if it asked for one.
    pub requested: Option<String>,
    /// Files tried and passed over, in the order tried.
    pub skipped: Vec<Skipped>,
}

impl Found {
    /// Whether the tuning is not the one the sensor named (the generic one or the defaults).
    pub fn is_fallback(&self) -> bool {
        !matches!(self.source, Source::File { .. })
    }

    /// One line for the log: the file used, and for a fallback, that the named tuning was not
    /// found and why the named files were passed over.
    pub fn describe(&self) -> String {
        let asked = self.requested.as_deref().unwrap_or("(none named)");
        let used = match &self.source {
            Source::File { path, origin } => {
                format!("tuning {} ({})", path.display(), origin.word())
            }
            Source::Generic { path, origin } => format!(
                "generic tuning {} ({}): no tuning named {asked} found",
                path.display(),
                origin.word()
            ),
            Source::Defaults => {
                format!("styx-aaa defaults: no tuning named {asked} found and no {GENERIC_NAME}")
            }
        };
        if self.skipped.is_empty() {
            return used;
        }
        let why: Vec<String> = self
            .skipped
            .iter()
            .map(|s| format!("{}: {}", s.path.display(), s.reason))
            .collect();
        format!("{used}; skipped {}", why.join("; "))
    }
}

impl TuningSearch {
    /// A search over `added` directories, then `bundled`.
    pub fn new(added: impl IntoIterator<Item = PathBuf>, bundled: Option<PathBuf>) -> Self {
        Self {
            added: added.into_iter().collect(),
            bundled,
        }
    }

    /// The search of a running system: [`TUNING_PATH_ENV`] (colon separated, empty entries
    /// dropped), then [`ADDED_DIR`], then [`BUNDLED_DIR`].
    pub fn system() -> Self {
        let mut added: Vec<PathBuf> = std::env::var_os(TUNING_PATH_ENV)
            .map(|v| {
                std::env::split_paths(&v)
                    .filter(|p| !p.as_os_str().is_empty())
                    .collect()
            })
            .unwrap_or_default();
        added.push(PathBuf::from(ADDED_DIR));
        Self::new(added, Some(PathBuf::from(BUNDLED_DIR)))
    }

    /// The directories in search order, with their origin.
    fn dirs(&self) -> impl Iterator<Item = (&Path, Origin)> {
        self.added
            .iter()
            .map(|d| (d.as_path(), Origin::Added))
            .chain(self.bundled.iter().map(|d| (d.as_path(), Origin::Bundled)))
    }

    /// The tuning named `name` (a sensor description's tuning file name, `None` for none), or the
    /// generic one, or the defaults. Says which, and passes over what did not load.
    pub fn find(&self, name: Option<&str>) -> Found {
        let mut skipped = Vec::new();
        let requested = name.map(str::to_owned);
        if let Some(name) = name {
            match plain_name(name) {
                Err(reason) => skipped.push(Skipped {
                    path: PathBuf::from(name),
                    reason,
                }),
                Ok(()) => {
                    for (dir, origin) in self.dirs() {
                        for file in candidates(name) {
                            let path = dir.join(&file);
                            if let Some(tuning) = load(&path, &mut skipped) {
                                return Found {
                                    tuning,
                                    source: Source::File { path, origin },
                                    requested,
                                    skipped,
                                };
                            }
                        }
                    }
                }
            }
        }
        for (dir, origin) in self.dirs() {
            let path = dir.join(GENERIC_NAME);
            if let Some(tuning) = load(&path, &mut skipped) {
                return Found {
                    tuning,
                    source: Source::Generic { path, origin },
                    requested,
                    skipped,
                };
            }
        }
        Found {
            tuning: Tuning::default(),
            source: Source::Defaults,
            requested,
            skipped,
        }
    }
}

/// Whether `name` is a plain file name: no directory part, not empty, not `.` or `..`.
fn plain_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty tuning name".into());
    }
    if name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name == "."
        || name == ".."
    {
        return Err("not a plain file name".into());
    }
    Ok(())
}

/// The file names tried for `name`, in order.
fn candidates(name: &str) -> Vec<String> {
    let path = Path::new(name);
    let is_json = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"));
    if is_json && let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        return vec![format!("{stem}.toml"), name.to_owned()];
    }
    vec![name.to_owned()]
}

/// The tuning in the file at `path`, if it exists and loads. A file that exists but does not
/// load is recorded in `skipped`. A missing file is not an entry.
fn load(path: &Path, skipped: &mut Vec<Skipped>) -> Option<Tuning> {
    if !path.is_file() {
        return None;
    }
    let skip = |skipped: &mut Vec<Skipped>, reason: String| {
        skipped.push(Skipped {
            path: path.to_path_buf(),
            reason,
        });
        None
    };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return skip(skipped, format!("unreadable: {e}")),
    };
    let is_toml = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("toml"));
    let is_json = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"));
    if is_json {
        return skip(
            skipped,
            "Raspberry Pi JSON is not read yet; only styx-aaa TOML loads".into(),
        );
    }
    if !is_toml {
        return skip(skipped, "unknown tuning format (expected .toml)".into());
    }
    let tuning: Tuning = match toml::from_str(&text) {
        Ok(t) => t,
        Err(e) => return skip(skipped, format!("not styx-aaa TOML: {e}")),
    };
    if let Err(e) = tuning.validate() {
        return skip(skipped, format!("invalid tuning: {e}"));
    }
    Some(tuning)
}
