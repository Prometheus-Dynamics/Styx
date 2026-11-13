//! The K230D's ISP8000 as Styx's ISP (`styx-k230`). Written, untested on board.
//!
//! Three parts, each in its own module:
//!
//! - [`settings`]: the neutral settings ([`styx_settings::IspSettings`]) mapped to the frame's
//!   register writes ([`Params`]), and the ISP's statistics mapped to the 3A's input
//!   ([`settings::statistics`]). The values are Styx's; the register addresses, field masks and
//!   the configuration layout are Lemnos's `k230isp` library, re-exported by `styx-lemnos`
//!   (feature `k230isp`). No register or struct of the ISP is written in this crate.
//! - [`params`]: the writes themselves, and their encoding into a configuration of Lemnos's
//!   `camera.isp/1` layout ([`Params::encode`]). Lemnos's `config::check` refuses a list with a
//!   write outside its table, so a list that passes it can reach the block.
//! - [`isp`]: the live ISP, posting jobs on the device's configuration stream and reading the
//!   statistics stream through `styx-lemnos`.
//!
//! # Gaps (outside blocks)
//!
//! - Lens shading is not written. Lemnos's `k230isp` library has the lens shading registers'
//!   addresses and masks, but no layout for the sample table, the sector sizes and the gradients,
//!   so Styx does not pack them. The dev branch packed them (`crates/k230isp/src/params.rs`,
//!   `lens_shading`); the table of 17 rows of nine words, and the sector layout, are the dev
//!   packing, unconfirmed. A `settings.lens_shading` is ignored on this ISP until Lemnos gives the
//!   block a layout.
//! - The statistics stream is opened as a frame stream of one row of [`STATISTICS_BYTES`] grey
//!   bytes: `styx-lemnos` has no read-only byte stream, so the frame's size is taken from the
//!   stream's own buffer size.

pub mod isp;
pub mod params;
pub mod settings;
pub mod stats;

pub use isp::{Error as IspError, Isp};
pub use params::{ConfigError, Params, RegWrite};
pub use settings::{params, pixels_per_block, statistics};
pub use styx_lemnos::k230isp::STATISTICS_BYTES;
pub use styx_lemnos::k230isp::regs;
pub use styx_lemnos::k230isp::stats::Raw;

#[cfg(test)]
mod tests;
