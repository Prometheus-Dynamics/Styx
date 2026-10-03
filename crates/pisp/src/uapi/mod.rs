//! `#[repr(C)]` mirrors of the PiSP uAPI structures.
//!
//! Provenance: written from the Linux headers the device runs (Raspberry Pi kernel 6.12):
//! `include/uapi/linux/media/raspberrypi/pisp_be_config.h`, `pisp_common.h` and the `rp1-cfe`
//! driver's `pisp_fe_config.h`, `pisp_statistics.h`, `pisp_types.h` (all
//! `GPL-2.0-only WITH Linux-syscall-note`, i.e. a userspace interface definition; only the
//! layouts, names and constants are mirrored here). Field names follow the headers; type names
//! are Rust-style (`struct pisp_fe_config` is [`FeConfig`]).

mod be;
mod be_config;
mod common;
mod fe;
mod layout;
mod stats;

pub use be::*;
pub use be_config::*;
pub use common::*;
pub use fe::*;
pub use stats::*;
