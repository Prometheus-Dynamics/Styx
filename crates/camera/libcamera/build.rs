//! Builds the libcamera C shim (`c_api/`) when the `libcamera` feature is on and pkg-config finds
//! libcamera. Without the library, the crate compiles a stub that reports libcamera as not
//! available: the build never needs libcamera to pass.

use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_libcamera)");
    println!("cargo:rerun-if-changed=c_api");
    if std::env::var_os("CARGO_FEATURE_LIBCAMERA").is_none() {
        return;
    }
    let library = match pkg_config::Config::new().probe("libcamera") {
        Ok(library) => library,
        Err(err) => {
            println!(
                "cargo:warning=styx-libcamera: libcamera not found by pkg-config ({err}); the backend reports it unavailable"
            );
            return;
        }
    };
    let mut build = cc::Build::new();
    build.cpp(true).flag("-std=c++17");
    for include in &library.include_paths {
        build.include(include);
        if let Some(parent) = include.parent() {
            build.include(parent);
        }
    }
    let dir = PathBuf::from("c_api");
    let mut sources: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("read c_api")
        .map(|entry| entry.expect("c_api entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "cpp"))
        .collect();
    sources.sort();
    for source in &sources {
        println!("cargo:rerun-if-changed={}", source.display());
        build.file(source);
    }
    build.compile("styx_libcamera_shim");
    println!("cargo:rustc-cfg=has_libcamera");
}
