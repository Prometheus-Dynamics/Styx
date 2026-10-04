//! The build-script side of compiled descriptions (feature `build`): validate description files
//! on the host and write their compiled form to `$OUT_DIR/styx-sensor/<name>.postcard`, for
//! [`include_description!`](crate::include_description) and
//! [`SensorDescription::from_postcard`]. Every problem of a description is reported (with its
//! path in the file) and fails the build.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{BUILTIN_DESCRIPTIONS, SensorDescription};

fn out_dir() -> Result<PathBuf, String> {
    let out = std::env::var_os("OUT_DIR").ok_or("OUT_DIR is not set: call this from build.rs")?;
    let dir = Path::new(&out).join("styx-sensor");
    fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Validates `toml` (labelled `source` in messages) and writes its compiled form into `dir`
/// under the sensor's name. Returns that name.
pub fn compile_str_to(dir: &Path, toml: &str, source: &str) -> Result<String, String> {
    let desc = SensorDescription::from_toml_str(toml, source).map_err(|e| e.to_string())?;
    let name = desc.sensor.name.clone();
    let path = dir.join(format!("{name}.postcard"));
    fs::write(&path, desc.to_postcard()).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(name)
}

/// Compiles description files (TOML) for `include_description!("<sensor name>")`, and tells
/// cargo to rerun the build script when they change. Call from `build.rs`.
pub fn compile<P: AsRef<Path>>(files: &[P]) -> Result<Vec<String>, String> {
    let dir = out_dir()?;
    files
        .iter()
        .map(|f| {
            let f = f.as_ref();
            println!("cargo:rerun-if-changed={}", f.display());
            let toml =
                fs::read_to_string(f).map_err(|e| format!("reading {}: {e}", f.display()))?;
            compile_str_to(&dir, &toml, &f.display().to_string())
        })
        .collect()
}

/// Compiles descriptions that ship with `styx-sensor` ([`BUILTIN_DESCRIPTIONS`]) by name.
pub fn compile_builtin(names: &[&str]) -> Result<(), String> {
    let dir = out_dir()?;
    for name in names {
        let (_, toml) = BUILTIN_DESCRIPTIONS
            .iter()
            .find(|(n, _)| n == name)
            .ok_or_else(|| format!("no built-in sensor description '{name}'"))?;
        compile_str_to(&dir, toml, name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_and_reports_every_problem() {
        let dir = std::env::temp_dir().join(format!("styx-sensor-build-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (_, toml) = BUILTIN_DESCRIPTIONS[0];
        let name = compile_str_to(&dir, toml, "ov9782.toml").unwrap();
        let bytes = fs::read(dir.join(format!("{name}.postcard"))).unwrap();
        let d = SensorDescription::from_postcard(&bytes).unwrap();
        assert_eq!(d, SensorDescription::from_toml_str(toml, "x").unwrap());
        let broken = toml.replace("address_bits = 16", "address_bits = 12");
        let e = compile_str_to(&dir, &broken, "broken.toml").unwrap_err();
        assert!(
            e.contains("broken.toml") && e.contains("sensor.address_bits"),
            "{e}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
