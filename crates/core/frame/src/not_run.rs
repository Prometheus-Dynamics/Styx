//! What a test does when a case cannot run on this host: the machine has no camera, no GPU, no
//! dma-heap it may allocate from, no PipeWire daemon, no test driver loaded. One rule for every
//! such case in the workspace, so that a case that did not run never reads as one that passed.
//!
//! A test calls [`not_run`] with what the case needs, the case's name and why it cannot run, and
//! then returns. What happens:
//!
//! - The need is required here (`STYX_REQUIRE_<NEED>=1`, the need in capitals with `_` for `-`:
//!   `STYX_REQUIRE_DMA_HEAP`, `STYX_REQUIRE_PIPEWIRE`, `STYX_REQUIRE_GPU`, ...): the case fails,
//!   naming itself and why. A host that should have the thing and does not is a failure.
//! - Otherwise the case is passed over, and said so: it prints `NOT RUN [<need>] <case>: <why>`
//!   and adds that line to the file `STYX_NOT_RUN_LOG` names, if any. `scripts/gate.sh` sets that
//!   file and reports, at the end and in CI's job summary, how many cases did not run and for
//!   what.
//!
//! A case whose code is not built at all (a feature that is off) is not this module's: such a
//! test is compiled out with `#[cfg]`, so that it is not listed as passed.

use std::fs::OpenOptions;
use std::io::Write;
use std::string::String;
use std::{env, format, println};

/// The environment variable naming the file each case that did not run adds a line to.
pub const LOG: &str = "STYX_NOT_RUN_LOG";

/// The environment variable that makes a case needing `need` fail when it cannot run.
#[must_use]
pub fn require_var(need: &str) -> String {
    let mut name = String::from("STYX_REQUIRE_");
    for c in need.chars() {
        name.push(if c.is_ascii_alphanumeric() {
            c.to_ascii_uppercase()
        } else {
            '_'
        });
    }
    name
}

/// Whether a case needing `need` must run on this host.
#[must_use]
pub fn required(need: &str) -> bool {
    env::var(require_var(need)).is_ok_and(|v| v == "1")
}

/// Case `case` needs `need` (a short lower-case word: `dma-heap`, `pipewire`, `camera`, `gpu`,
/// `vivid`, `cc`) and cannot run here, for `why`: fails it when the need is required, or records
/// that it did not run. The caller returns after this.
///
/// # Panics
///
/// When the need is required here.
pub fn not_run(need: &str, case: &str, why: &str) {
    if required(need) {
        panic!(
            "{case} must run ({}=1) and cannot: {why}",
            require_var(need)
        );
    }
    let line = format!("NOT RUN [{need}] {case}: {why}");
    println!("{line}");
    if let Some(path) = env::var_os(LOG) {
        // One write of one line, appended: the test binaries of a gate run at once.
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = file.write_all(format!("{line}\n").as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_require_variable_is_the_need_in_capitals() {
        assert_eq!(require_var("dma-heap"), "STYX_REQUIRE_DMA_HEAP");
        assert_eq!(require_var("pipewire"), "STYX_REQUIRE_PIPEWIRE");
        assert_eq!(require_var("gpu"), "STYX_REQUIRE_GPU");
    }
}
