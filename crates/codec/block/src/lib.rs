//! Hardware codec blocks for Styx: the HEVC bitstream and the decode jobs that run on a block.
//!
//! Lemnos owns the block (its `codec.block/1` contract, and the M2M session under the
//! stateless driver); Styx owns the bitstream. So the parser ([`hevc`]) and the controls it
//! builds ([`hevc::controls`]) live here, and the job that carries them is built by [`work`].
//! [`decoder::HevcDecoder`] runs each frame on a [`decoder::Session`], which is where a block
//! is reached, and [`decoder::candidate`] registers it with `styx-codec-api`'s [`Registry`]
//! as a native decoder.
//!
//! - Decoding is stateless HEVC, one IDR slice per frame, Annex B in, NV12 out. Inter slices
//!   are refused: their reference picture sets are not parsed yet.
//! - Recovery, with the backoff of Lemnos's stateless decoder, lives in [`decoder`].
//! - H.264 decodes by slice as well ([`h264`]): its parser, reference tracker and controls are
//!   here, and its controls are Lemnos's V4L2 H.264 structures.
//! - Encoding is not here yet.
//! - Each job is stamped by its decoder, a whole number of microseconds and unique ([`work::Stamps`]),
//!   and a reference picture is named in the controls by its stamp, never as a slot.
//!
//! No `unsafe` in this crate. The one the kernel's structures need is in `lemnos-linux-sys`,
//! and the byte views of the controls are its safe `as_bytes`.
//!
//! [`Registry`]: styx_codec_api::Registry

pub mod bitstream;
pub mod decoder;
pub mod h264;
pub mod hevc;
pub mod link;
pub mod live;
pub mod work;

pub use decoder::{HevcDecoder, Session, candidate, descriptor};
pub use link::backoff_ns;
pub use live::LiveSession;
