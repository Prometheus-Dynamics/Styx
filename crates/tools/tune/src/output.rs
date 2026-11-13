//! Writing a tuning where Styx's tuning search finds it.
//!
//! The file is styx-aaa's TOML (what `styx-tuning` reads) named `<name>.toml`, in a directory
//! the search reads as an added directory (`STYX_TUNING_PATH`, or `/etc/styx/tuning`, or a
//! directory the caller names). The bundled install directory is refused: it belongs to the
//! package.

use std::path::{Path, PathBuf};

use crate::Tuning;
use crate::error::{Result, TuneError};

/// The tuning as styx-aaa TOML (validated first).
pub fn to_toml_string(tuning: &Tuning) -> Result<String> {
    tuning.validate()?;
    toml::to_string(tuning).map_err(|e| TuneError::Format(format!("tuning as TOML: {e}")))
}

/// Write `tuning` as `<dir>/<name>.toml` and return the path. `name` is a plain file name
/// (the sensor's tuning name); `dir` must not be the bundled directory.
pub fn write(dir: &Path, name: &str, tuning: &Tuning) -> Result<PathBuf> {
    if name.is_empty()
        || name.contains(['/', '\\', '\0'])
        || name == "."
        || name == ".."
        || name.ends_with(".toml")
    {
        return Err(TuneError::Session(format!(
            "{name:?} is not a plain tuning name (give it without .toml)"
        )));
    }
    if dir == Path::new(styx_tuning::BUNDLED_DIR) {
        return Err(TuneError::Session(format!(
            "{} is the bundled install directory; write to an added directory",
            dir.display()
        )));
    }
    let text = to_toml_string(tuning)?;
    std::fs::create_dir_all(dir).map_err(|e| TuneError::io(dir, e))?;
    let path = dir.join(format!("{name}.toml"));
    std::fs::write(&path, text).map_err(|e| TuneError::io(&path, e))?;
    Ok(path)
}
