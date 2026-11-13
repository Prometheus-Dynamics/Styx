//! Builds the GStreamer element's C shim (`c_api/styx_gst.c`) when the `gstreamer` feature is on
//! and pkg-config finds the libraries. Without them the crate compiles without the element and
//! says so as a build warning; the build never needs GStreamer to pass.

use std::path::PathBuf;

fn probe(name: &str) -> Option<pkg_config::Library> {
    match pkg_config::Config::new().probe(name) {
        Ok(library) => Some(library),
        Err(err) => {
            println!(
                "cargo:warning=styx-gst: {name} not found by pkg-config ({err}); styxsrc is left out"
            );
            None
        }
    }
}

fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_gstreamer)");
    println!("cargo::rustc-check-cfg=cfg(has_gst_allocators)");
    println!("cargo:rerun-if-changed=c_api");
    if std::env::var_os("CARGO_FEATURE_GSTREAMER").is_none() {
        return;
    }
    let libraries: Option<Vec<pkg_config::Library>> =
        ["gstreamer-1.0", "gstreamer-base-1.0", "gstreamer-video-1.0"]
            .iter()
            .map(|name| probe(name))
            .collect();
    let Some(libraries) = libraries else { return };
    // dma-buf memory for frames that are dma-bufs: optional, the element wraps memory without it.
    let allocators = probe("gstreamer-allocators-1.0");

    let mut build = cc::Build::new();
    build.file("c_api/styx_gst.c");
    let mut includes: Vec<PathBuf> = Vec::new();
    for library in libraries.iter().chain(allocators.iter()) {
        includes.extend(library.include_paths.iter().cloned());
    }
    for include in &includes {
        build.include(include);
    }
    if allocators.is_some() {
        build.define("STYX_HAVE_ALLOCATORS", None);
        println!("cargo:rustc-cfg=has_gst_allocators");
    }
    build.warnings(true).compile("styx_gst_shim");
    println!("cargo:rustc-cfg=has_gstreamer");
}
