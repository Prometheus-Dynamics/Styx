//! Styx's own baseline JPEG encoder: one implementation, planar YUV or grey in, JFIF out.
//!
//! - [`JpegEncoder`] is the encoder over [`PlanarInput`]s: the caller's planes, colour from
//!   [`styx-simd`](styx_simd)'s JFIF and DCT kernels (RVV on RISC-V, the scalar oracle elsewhere),
//!   Huffman coding with the typical tables of T.81 Annex K. Output goes to a `Vec` or to a
//!   caller's buffer ([`JpegEncoder::encode_fixed`]).
//! - [`StyxJpegEncoder`] is that encoder as a `styx-codec-api` [`Encoder`](styx_codec_api::Encoder)
//!   from I420 (YU12), NV12, GREY or R8 frames to MJPG frames, with the native backend. [`candidate`]
//!   gives it to a [`Registry`](styx_codec_api::Registry).
//! - [`JpegEncoder::encode_blocks`] codes blocks computed elsewhere (a GPU's DCT, later) in the
//!   [`CoefficientGrid`] layout, to the same bytes.
//!
//! Restart markers after every MCU row ([`JpegEncoder::with_restart_rows`]) make each row an
//! independently decodable slice; without them the bytes carry none.

mod codec;
mod encoder;
mod entropy;
mod tables;

pub use codec::{INPUTS, StyxJpegEncoder, candidate, descriptor};
pub use encoder::{CoefficientGrid, JpegEncoder, PlanarInput};
