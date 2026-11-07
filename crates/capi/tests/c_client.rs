//! A C program on the surface (`tests/c/client.c`), linked against the static library and
//! against the shared one.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where cargo put this crate's static and shared libraries: beside the test binary's
/// directory (`deps/..`).
fn libs() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().to_path_buf()
}

/// The libraries come from `cargo build -p styx-capi`, which `cargo test` does not run: the gate
/// builds them first and sets `STYX_CAPI_C_REQUIRED=1`, so there a missing one fails.
fn skip(why: &str) {
    assert!(
        std::env::var_os("STYX_CAPI_C_REQUIRED").is_none(),
        "STYX_CAPI_C_REQUIRED and {why}"
    );
    #[allow(clippy::print_stderr)] // a skipped test says why
    {
        eprintln!("skipped: {why} (build it with cargo build -p styx-capi)");
    }
}

fn have_cc() -> bool {
    Command::new("cc")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn build(out: &Path, link: &[String]) {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let result = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Werror", "-I"])
        .arg(format!("{manifest}/include"))
        .arg(format!("{manifest}/tests/c/client.c"))
        .arg("-o")
        .arg(out)
        .args(link)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "cc: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn run(program: &Path) {
    let out = Command::new(program).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("ok frames=5"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(all(target_os = "linux", not(feature = "shared")))]
#[test]
fn a_c_program_takes_frames_through_the_shared_library() {
    let dir = libs();
    let lib = dir.join("libstyx_capi.so");
    if !have_cc() || !lib.exists() {
        return skip(&format!("no cc or no {}", lib.display()));
    }
    // The program asks for the SONAME, libstyx_capi.so.1, as an installed one would.
    let tmp = std::env::temp_dir().join(format!("styx-capi-c-shared-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let so = tmp.join("libstyx_capi.so.1");
    let _ = std::fs::remove_file(&so);
    std::os::unix::fs::symlink(&lib, &so).unwrap();
    let out = tmp.join("client");
    build(
        &out,
        &[
            format!("-L{}", dir.display()),
            "-lstyx_capi".into(),
            format!("-Wl,-rpath,{}", tmp.display()),
        ],
    );
    run(&out);
    let _ = std::fs::remove_dir_all(tmp);
}

#[cfg(all(target_os = "linux", not(feature = "shared")))]
#[test]
fn a_c_program_takes_frames_through_the_static_library() {
    let dir = libs();
    let lib = dir.join("libstyx_capi.a");
    if !have_cc() || !lib.exists() {
        return skip(&format!("no cc or no {}", lib.display()));
    }
    let out = std::env::temp_dir().join(format!("styx-capi-c-static-{}", std::process::id()));
    build(
        &out,
        &[
            lib.display().to_string(),
            "-lpthread".into(),
            "-ldl".into(),
            "-lm".into(),
            "-lrt".into(),
            "-lgcc_s".into(),
            "-lc".into(),
        ],
    );
    run(&out);
    let _ = std::fs::remove_file(out);
}
