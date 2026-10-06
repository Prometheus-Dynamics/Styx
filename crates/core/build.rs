//! With the `daedalus` feature: exports the enabled features as `DAEDALUS_CRATE_FEATURES` for the
//! `styx.frames` plugin's `#[plugin(.., crate_build)]` (`daedalus::crate_build_info!()`), so a
//! dynamic plugin's boundary conflict on `styx:framelease` names the Styx features that differ.
//! Other builds do nothing here.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_DAEDALUS").is_some() {
        let features = std::env::var("CARGO_CFG_FEATURE").unwrap_or_default();
        println!("cargo:rustc-env=DAEDALUS_CRATE_FEATURES={features}");
    }
}
