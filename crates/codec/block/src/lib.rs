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
//! - Encoding is not here yet, and H.264 is not moved: Lemnos has no H.264 parser at its lock.
//!
//! No `unsafe` in this crate. The one the kernel's structures need is in `lemnos-linux-sys`,
//! and the byte views of the controls are its safe `as_bytes`.
//!
//! [`Registry`]: styx_codec_api::Registry

pub mod decoder;
pub mod hevc;
pub mod work;

pub use decoder::{HevcDecoder, Session, backoff_ns, candidate, descriptor};
