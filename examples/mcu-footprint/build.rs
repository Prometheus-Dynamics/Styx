//! Compiles the built-in OV9782 description for `include_description!`, and for bare-metal ARM
//! targets gives the firmware images cortex-m-rt's linker script and a memory map.

use std::env;
use std::fs;
use std::path::PathBuf;

/// Large enough to link every configuration (an STM32H7's 2 MB of flash and 1 MB of RAM);
/// what fits on smaller parts is read from the section sizes (docs/mcu.md).
const MEMORY: &str = "MEMORY
{
  FLASH : ORIGIN = 0x08000000, LENGTH = 2048K
  RAM : ORIGIN = 0x20000000, LENGTH = 1024K
}
";

fn main() {
    if let Err(e) = styx_sensor::build::compile_builtin(&["ov9782"]) {
        panic!("{e}");
    }
    let target = env::var("TARGET").unwrap_or_default();
    if target.starts_with("thumb") && target.contains("-none-") {
        let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
        fs::write(out.join("memory.x"), MEMORY).expect("memory.x");
        println!("cargo:rustc-link-search={}", out.display());
        println!("cargo:rustc-link-arg-bins=--nmagic");
        println!("cargo:rustc-link-arg-bins=-Tlink.x");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
