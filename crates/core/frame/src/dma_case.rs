//! What a test does when its dma-buf case cannot run on this host (no dma-heap it may allocate
//! from, no `/dev/udmabuf`): one rule for every such case in the workspace, so a gate that did not
//! run the dma-buf path never reads as having tested it.
//!
//! - `STYX_REQUIRE_DMA_HEAP=1` (what `scripts/gate.sh` sets unless told otherwise): the case
//!   fails, naming itself and why. A host that should have a heap and does not is a failure.
//! - Otherwise the case is passed over: it prints `SKIPPED dma-buf case <case>: <why>`, and adds
//!   that line to the file `STYX_DMA_SKIP_LOG` names, if any, from which the gate reports how
//!   many cases were skipped.
//!
//! A case skipped for something else (no GPU device, say) does not come here.

use std::fs::OpenOptions;
use std::io::Write;
use std::{env, format, println};

/// The environment variable that makes a dma-buf case that cannot run fail.
pub const REQUIRE: &str = "STYX_REQUIRE_DMA_HEAP";
/// The environment variable naming the file each skipped case adds a line to.
pub const SKIP_LOG: &str = "STYX_DMA_SKIP_LOG";

/// Case `case` cannot run here, for `why`: fails it under [`REQUIRE`], or records the skip.
///
/// # Panics
///
/// When [`REQUIRE`] is `1`.
pub fn unavailable(case: &str, why: &str) {
    if env::var(REQUIRE).is_ok_and(|v| v == "1") {
        panic!("dma-buf case {case} must run ({REQUIRE}=1) and cannot: {why}");
    }
    let line = format!("SKIPPED dma-buf case {case}: {why}");
    println!("{line}");
    if let Some(path) = env::var_os(SKIP_LOG) {
        // One write of one line, appended: the test binaries of a gate run at once.
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = file.write_all(format!("{line}\n").as_bytes());
        }
    }
}
