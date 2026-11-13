//! Finds the system libturbojpeg when the `turbojpeg` feature is on. Sets `styx_turbojpeg` only
//! when the library is there; without it the turbojpeg code is compiled out and the decoder
//! reports itself unavailable. Nothing is built from source and no system package is required.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(styx_turbojpeg)");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_PATH");
    if std::env::var_os("CARGO_FEATURE_TURBOJPEG").is_none() {
        return;
    }
    if found_with_pkg_config() || found_in_system_dirs() {
        println!("cargo::rustc-cfg=styx_turbojpeg");
        println!("cargo::rustc-link-lib=turbojpeg");
    } else {
        println!(
            "cargo::warning=libturbojpeg not found; the turbojpeg feature is compiled out (zune-jpeg is used)"
        );
    }
}

fn found_with_pkg_config() -> bool {
    let Ok(status) = Command::new("pkg-config")
        .args(["--exists", "libturbojpeg"])
        .status()
    else {
        return false;
    };
    if !status.success() {
        return false;
    }
    if let Ok(out) = Command::new("pkg-config")
        .args(["--libs-only-L", "libturbojpeg"])
        .output()
    {
        for token in String::from_utf8_lossy(&out.stdout).split_whitespace() {
            if let Some(dir) = token.strip_prefix("-L") {
                println!("cargo::rustc-link-search=native={dir}");
            }
        }
    }
    true
}

fn found_in_system_dirs() -> bool {
    [
        "/usr/lib64",
        "/usr/lib",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/local/lib",
    ]
    .iter()
    .any(|dir| Path::new(dir).join("libturbojpeg.so").exists())
}
