//! Builds the PipeWire node's C shim (`c_api/styx_pipewire.c`) when the `pipewire` feature is on and
//! pkg-config finds libpipewire-0.3. Without the library the crate builds with the node left out
//! and says so as a build warning; the build never needs PipeWire to pass.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_pipewire)");
    println!("cargo:rerun-if-changed=c_api");
    if std::env::var_os("CARGO_FEATURE_PIPEWIRE").is_none() {
        return;
    }
    let library = match pkg_config::Config::new().probe("libpipewire-0.3") {
        Ok(library) => library,
        Err(err) => {
            println!(
                "cargo:warning=styx-pipewire: libpipewire-0.3 not found by pkg-config ({err}); the node is left out"
            );
            return;
        }
    };
    let mut build = cc::Build::new();
    build.file("c_api/styx_pipewire.c");
    for include in &library.include_paths {
        build.include(include);
    }
    build.warnings(true).compile("styx_pipewire_shim");
    println!("cargo:rustc-cfg=has_pipewire");
}
