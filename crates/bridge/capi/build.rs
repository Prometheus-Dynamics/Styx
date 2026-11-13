//! Gives the shared library a SONAME with the surface's major version, `libstyx_capi.so.<major>`,
//! so the loader refuses a library of another major. The major comes from `include/styx.h`, the
//! one place it is written.

fn main() {
    println!("cargo:rerun-if-changed=include/styx.h");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    let header = std::fs::read_to_string("include/styx.h").expect("include/styx.h");
    let major = header
        .lines()
        .find_map(|line| line.strip_prefix("#define STYX_ABI_MAJOR "))
        .map(str::trim)
        .expect("STYX_ABI_MAJOR in include/styx.h");
    assert!(
        !major.is_empty() && major.bytes().all(|b| b.is_ascii_digit()),
        "STYX_ABI_MAJOR is not a number: {major:?}"
    );
    println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libstyx_capi.so.{major}");
}
