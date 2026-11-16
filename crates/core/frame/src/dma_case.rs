//! What a test does when its dma-buf case cannot run on this host (no dma-heap it may allocate
//! from, no `/dev/udmabuf`): [`crate::not_run`]'s rule, with the need `dma-heap`.
//!
//! - `STYX_REQUIRE_DMA_HEAP=1` (what `scripts/gate.sh` sets unless told otherwise): the case
//!   fails, naming itself and why. A host that should have a heap and does not is a failure.
//! - Otherwise the case is passed over and recorded as not run, and the gate reports it.

/// The environment variable that makes a dma-buf case that cannot run fail.
pub const REQUIRE: &str = "STYX_REQUIRE_DMA_HEAP";

/// Case `case` cannot run here, for `why`: fails it under [`REQUIRE`], or records that it did not
/// run.
///
/// # Panics
///
/// When [`REQUIRE`] is `1`.
pub fn unavailable(case: &str, why: &str) {
    crate::not_run::not_run("dma-heap", case, why);
}
