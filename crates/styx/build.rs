//! With `replay-softisp`: the built-in sensor descriptions compiled to their binary form
//! (`styx_sensor::build`, validated here), so a replay loads them without the TOML
//! deserializers for descriptions in the binary.

fn main() {
    #[cfg(feature = "replay-softisp")]
    {
        let names: Vec<&str> = styx_sensor::BUILTIN_DESCRIPTIONS
            .iter()
            .map(|(name, _)| *name)
            .collect();
        styx_sensor::build::compile_builtin(&names).expect("built-in sensor descriptions");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
