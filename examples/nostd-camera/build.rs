//! Compiles the built-in OV9782 description for `include_description!` (validated here: a broken
//! description fails this build).

fn main() {
    if let Err(e) = styx_sensor::build::compile_builtin(&["ov9782"]) {
        panic!("{e}");
    }
}
